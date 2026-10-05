//! Voice huddles: this agent joins a relay audio room as itself and brings its own
//! live voice model. goose negotiates the model (`live-voice/start`); this harness
//! owns the relay socket and the WebRTC media endpoint.
//!
//! Media stays Opus end to end and is never decoded. Peers' 20 ms relay frames go
//! to the model as RTP samples, one active speaker at a time (no mixing). The
//! model's RTP payloads become relay frames.
use crate::{acp::McpServer, config::Config, runtime::PoolStartup, AcpClient};
use anyhow::{anyhow, bail, Context, Result};
use futures_util::{SinkExt, StreamExt};
use nostr::{EventBuilder, Kind, Tag};
use rtc::media::Sample;
use rtc::media_stream::MediaStreamTrack;
use rtc::rtp_transceiver::rtp_sender::{
    RTCRtpCodec, RTCRtpCodecParameters, RTCRtpCodingParameters, RTCRtpEncodingParameters,
    RtpCodecKind,
};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, oneshot};
use tokio_tungstenite::{connect_async, tungstenite::Message};
use uuid::Uuid;
use webrtc::media_stream::track_local::static_sample::TrackLocalStaticSample;
use webrtc::media_stream::track_local::TrackLocal;
use webrtc::media_stream::track_remote::{TrackRemote, TrackRemoteEvent};
use webrtc::peer_connection::{
    MediaEngine, PeerConnection, PeerConnectionBuilder, PeerConnectionEventHandler,
    RTCConfigurationBuilder, RTCIceGatheringState, RTCPeerConnectionState, RTCSessionDescription,
};

/// Same wire contract as human clients: protocol v2, 48 kHz mono Opus, 20 ms.
const PROTOCOL_VERSION: u8 = 2;
const FRAME_SAMPLES: u32 = 960;
const FRAME: Duration = Duration::from_millis(20);
const OPUS_PT: u8 = 111;
const SPEAKING_DBOV: i8 = -55;
const FLOOR_HOLD: Duration = Duration::from_millis(600);
const HANDSHAKE: Duration = Duration::from_secs(10);
/// Huddle starts older than the backing channel's TTL are over.
const HUDDLE_TTL_SECS: u64 = 3600;

/// A huddle's backing channel and the parent channel its start event names.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Huddle {
    pub backing: Uuid,
    pub parent: Uuid,
}

/// The relay links a backing channel to its parent only through the creator's
/// kind-48100 start. Returns `None` for ordinary channels.
pub(crate) fn find_start(events: &Value, backing: Uuid) -> Option<Huddle> {
    events.as_array()?.iter().find_map(|event| {
        let content: Value = serde_json::from_str(event["content"].as_str()?).ok()?;
        let named = content["ephemeral_channel_id"]
            .as_str()?
            .parse::<Uuid>()
            .ok()?;
        let parent = event["tags"].as_array()?.iter().find(|tag| tag[0] == "h")?[1]
            .as_str()?
            .parse::<Uuid>()
            .ok()?;
        (event["kind"] == 48100 && named == backing).then_some(Huddle { backing, parent })
    })
}

pub(crate) fn start_filter() -> Value {
    let since = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |now| now.as_secs().saturating_sub(HUDDLE_TTL_SECS));
    json!({ "kinds": [48100], "since": since, "limit": 50 })
}

/// Relay → client: `peer u8 | seq u16 | ts u32 | dBov i8 | flags u8 | opus`.
fn parse_frame(data: &[u8]) -> Option<(u8, i8, &[u8])> {
    (data.len() > 9).then(|| (data[0], data[7] as i8, &data[9..]))
}
/// Client → relay: the same 8-byte header without the peer prefix.
fn encode_frame(seq: u16, timestamp: u32, opus: &[u8]) -> Vec<u8> {
    // Without decoding, size is the speaking signal: DTX/comfort noise is tiny.
    let speaking = opus.len() > 20;
    let mut frame = Vec::with_capacity(8 + opus.len());
    frame.extend_from_slice(&seq.to_be_bytes());
    frame.extend_from_slice(&timestamp.to_be_bytes());
    frame.push(if speaking { -20i8 } else { -127i8 } as u8);
    frame.push(u8::from(opus.len() <= 2));
    frame.extend_from_slice(opus);
    frame
}

/// Choose one speaker for the model: keep the current holder until it has been
/// quiet for `FLOOR_HOLD`, then admit the next peer that speaks.
#[derive(Default)]
struct Floor {
    holder: Option<(u8, Instant)>,
}
impl Floor {
    fn admit(&mut self, peer: u8, level: i8, now: Instant) -> bool {
        let loud = level > SPEAKING_DBOV;
        match self.holder {
            Some((holder, _)) if holder == peer => {
                if loud {
                    self.holder = Some((peer, now));
                }
                true
            }
            Some((_, last)) if now.duration_since(last) < FLOOR_HOLD => false,
            _ if loud => {
                self.holder = Some((peer, now));
                true
            }
            _ => false,
        }
    }
}

struct Events {
    gathered: mpsc::Sender<()>,
    failed: mpsc::Sender<()>,
    model: mpsc::Sender<bytes::Bytes>,
}
#[async_trait::async_trait]
impl PeerConnectionEventHandler for Events {
    async fn on_ice_gathering_state_change(&self, state: RTCIceGatheringState) {
        if state == RTCIceGatheringState::Complete {
            let _ = self.gathered.try_send(());
        }
    }
    async fn on_connection_state_change(&self, state: RTCPeerConnectionState) {
        if matches!(
            state,
            RTCPeerConnectionState::Failed | RTCPeerConnectionState::Closed
        ) {
            let _ = self.failed.try_send(());
        }
    }
    async fn on_track(&self, track: Arc<dyn TrackRemote>) {
        let model = self.model.clone();
        tokio::spawn(async move {
            while let Some(event) = track.poll().await {
                if let TrackRemoteEvent::OnRtpPacket(packet) = event {
                    // Bounded: drop model audio rather than queue latency.
                    let _ = model.try_send(packet.payload);
                }
            }
        });
    }
}

/// Everything the bridge needs from the harness, cloned once at join time.
pub(crate) struct Launch {
    pub startup: PoolStartup,
    pub mcp_servers: Vec<McpServer>,
    pub cwd: String,
    pub system_prompt: Option<String>,
    pub keys: nostr::Keys,
    pub relay_url: String,
    pub auth_tag: Option<Tag>,
}
impl Launch {
    pub(crate) fn new(
        config: &Config,
        startup: PoolStartup,
        auth_tag: Option<Tag>,
    ) -> Result<Self> {
        Ok(Self {
            startup,
            mcp_servers: crate::build_mcp_servers(config),
            cwd: crate::current_working_directory()?,
            system_prompt: config.system_prompt.clone(),
            keys: config.keys.clone(),
            relay_url: config.relay_url.trim_end_matches('/').to_owned(),
            auth_tag,
        })
    }
}

/// Join `backing` if it is a live huddle's backing channel; otherwise do nothing.
pub(crate) async fn join_if_huddle(
    rest: crate::relay::RestClient,
    launch: Result<Launch>,
    backing: Uuid,
    stop: oneshot::Receiver<()>,
) {
    let huddle = match rest.query_raw(&[start_filter()]).await {
        Ok(events) => find_start(&events, backing),
        Err(error) => {
            tracing::warn!(channel = %backing, "huddle: start lookup failed: {error}");
            return;
        }
    };
    let Some(huddle) = huddle else { return };
    let result = match launch {
        Ok(launch) => run(launch, huddle, stop).await,
        Err(error) => Err(error),
    };
    if let Err(error) = result {
        tracing::warn!(backing = %huddle.backing, "huddle: {error:#}");
    }
}

/// Join until the huddle ends, the agent is removed (`stop` resolves or drops), or
/// either connection fails. One dedicated agent process per huddle, like isolated tasks.
pub(crate) async fn run(
    launch: Launch,
    huddle: Huddle,
    mut stop: oneshot::Receiver<()>,
) -> Result<()> {
    // Relay first: a refused admission costs no model session.
    let url = format!("{}/huddle/{}/audio", launch.relay_url, huddle.backing);
    let (mut socket, _) = tokio::time::timeout(HANDSHAKE, connect_async(url.as_str()))
        .await
        .context("audio connect timed out")??;
    let peers = tokio::time::timeout(HANDSHAKE, authenticate(&mut socket, &launch, huddle))
        .await
        .context("audio handshake timed out")??;
    let mut peers: HashMap<u8, String> = peers;
    tracing::info!(backing = %huddle.backing, peers = peers.len(), "huddle: joined audio room");

    let (failed_tx, mut failed) = mpsc::channel(1);
    let (model_tx, mut model_audio) = mpsc::channel(64);
    let (pc, to_model, ssrc, offer) = media_endpoint(failed_tx, model_tx).await?;

    let mut agent = AcpClient::spawn_with_env(
        &launch.startup.command,
        &launch.startup.args,
        &launch.startup.extra_env,
        launch.startup.has_generated_codex_config,
        // goose live voice is opt-in, and needs autonomous mode and the unrolled
        // agent loop (also used for the work it delegates in this process).
        &[
            ("GOOSE_LIVE_VOICE_ENABLED".into(), "true".into()),
            ("GOOSE_MODE".into(), "auto".into()),
            ("GOOSE_STATE_MACHINE".into(), "true".into()),
        ],
    )
    .await?;
    let result = async {
        agent.initialize().await?;
        let session = agent
            .session_new(
                &launch.cwd,
                launch.mcp_servers.clone(),
                None,
                Some("Huddle"),
            )
            .await?;
        if let Some(prompt) = &launch.system_prompt {
            agent
                .session_set_goose_system_prompt(&session, prompt)
                .await?;
        }
        let (interaction, answer) = agent.live_voice_start(&session, &offer).await?;
        anyhow::Ok((session, interaction, answer))
    }
    .await;
    let (session, interaction, answer) = match result {
        Ok(started) => started,
        Err(error) => {
            agent.shutdown().await;
            let _ = pc.close().await;
            return Err(error.context("agent live voice did not start"));
        }
    };
    pc.set_remote_description(RTCSessionDescription::answer(answer)?)
        .await?;
    tracing::info!(backing = %huddle.backing, "huddle: live voice connected");

    let mut floor = Floor::default();
    let (mut seq, mut timestamp) = (0u16, 0u32);
    let outcome: Result<()> = loop {
        tokio::select! {
            _ = &mut stop => break Ok(()),
            _ = failed.recv() => break Err(anyhow!("model connection closed")),
            error = agent.serve() => break Err(anyhow!("agent exited: {error}")),
            Some(opus) = model_audio.recv() => {
                let frame = encode_frame(seq, timestamp, &opus);
                (seq, timestamp) = (seq.wrapping_add(1), timestamp.wrapping_add(FRAME_SAMPLES));
                socket.send(Message::Binary(frame.into())).await?;
            }
            message = socket.next() => match message {
                Some(Ok(Message::Binary(data))) => {
                    let Some((peer, level, opus)) = parse_frame(&data) else { continue };
                    if peers.contains_key(&peer) && floor.admit(peer, level, Instant::now()) {
                        let sample = Sample {
                            data: bytes::Bytes::copy_from_slice(opus),
                            duration: FRAME,
                            ..Sample::new(Instant::now())
                        };
                        // Media errors before ICE completes are expected; keep the room.
                        let _ = to_model.write_sample(ssrc, OPUS_PT, &sample, &[]).await;
                    }
                }
                Some(Ok(Message::Text(text))) => {
                    if let Err(error) = apply_control(&text, &mut peers) {
                        break Err(error);
                    }
                }
                Some(Ok(_)) => {}
                Some(Err(error)) => break Err(error.into()),
                None => break Ok(()), // the room ended
            },
        }
    };
    let _ = socket
        .send(Message::Text(r#"{"type":"leave"}"#.into()))
        .await;
    let _ = socket.close(None).await;
    let _ = agent.live_voice_stop(&session, &interaction).await;
    agent.shutdown().await;
    let _ = pc.close().await;
    tracing::info!(backing = %huddle.backing, "huddle: left");
    outcome
}

/// One send/receive Opus track and a fully gathered offer (goose relays it without trickle).
async fn media_endpoint(
    failed: mpsc::Sender<()>,
    model: mpsc::Sender<bytes::Bytes>,
) -> Result<(
    impl PeerConnection,
    Arc<TrackLocalStaticSample>,
    u32,
    String,
)> {
    let (gathered_tx, mut gathered) = mpsc::channel(1);
    let codec = RTCRtpCodec {
        mime_type: "audio/opus".into(),
        clock_rate: 48_000,
        channels: 2,
        sdp_fmtp_line: "minptime=20;useinbandfec=1".into(),
        rtcp_feedback: vec![],
    };
    let mut media = MediaEngine::default();
    media.register_codec(
        RTCRtpCodecParameters {
            rtp_codec: codec.clone(),
            payload_type: OPUS_PT,
        },
        RtpCodecKind::Audio,
    )?;
    let pc = PeerConnectionBuilder::new()
        .with_configuration(RTCConfigurationBuilder::new().build())
        .with_media_engine(media)
        .with_handler(Arc::new(Events {
            gathered: gathered_tx,
            failed,
            model,
        }))
        .with_udp_addrs(vec!["0.0.0.0:0"])
        .build()
        .await?;
    let ssrc = rand_ssrc();
    let track = Arc::new(TrackLocalStaticSample::new(
        Instant::now(),
        MediaStreamTrack::new(
            "huddle".into(),
            "huddle-audio".into(),
            "huddle".into(),
            RtpCodecKind::Audio,
            vec![RTCRtpEncodingParameters {
                rtp_coding_parameters: RTCRtpCodingParameters {
                    ssrc: Some(ssrc),
                    ..Default::default()
                },
                codec,
                ..Default::default()
            }],
        ),
    )?);
    pc.add_track(Arc::clone(&track) as Arc<dyn TrackLocal>)
        .await?;
    let offer = pc.create_offer(None).await?;
    pc.set_local_description(offer).await?;
    tokio::time::timeout(HANDSHAKE, gathered.recv())
        .await
        .context("ICE gathering timed out")?;
    let offer = pc
        .local_description()
        .await
        .ok_or_else(|| anyhow!("no local offer"))?
        .sdp;
    Ok((pc, track, ssrc, offer))
}

async fn authenticate<S>(
    socket: &mut S,
    launch: &Launch,
    huddle: Huddle,
) -> Result<HashMap<u8, String>>
where
    S: futures_util::Sink<Message, Error = tokio_tungstenite::tungstenite::Error>
        + futures_util::Stream<Item = Result<Message, tokio_tungstenite::tungstenite::Error>>
        + Unpin,
{
    let me = launch.keys.public_key().to_hex();
    while let Some(message) = socket.next().await {
        let Message::Text(text) = message? else {
            continue;
        };
        let control: Value = serde_json::from_str(&text)?;
        match control["type"].as_str() {
            Some("challenge") => {
                let challenge = control["challenge"].as_str().unwrap_or_default();
                // The `relay` tag is the community base URL, not the huddle URL.
                let mut tags = vec![
                    Tag::parse(["relay", launch.relay_url.as_str()])?,
                    Tag::parse(["challenge", challenge])?,
                ];
                tags.extend(launch.auth_tag.clone());
                let event = EventBuilder::new(Kind::Authentication, "")
                    .tags(tags)
                    .sign_with_keys(&launch.keys)?;
                let auth = json!({
                    "type": "auth",
                    "event": event,
                    "parent_channel_id": huddle.parent,
                    "protocol_version": PROTOCOL_VERSION,
                });
                socket.send(Message::Text(auth.to_string().into())).await?;
            }
            Some("joined") if control["pubkey"] == me.as_str() => {
                let mut peers = HashMap::new();
                apply_peers(&control["peers"], &mut peers);
                peers.retain(|_, pubkey| *pubkey != me);
                return Ok(peers);
            }
            Some("error" | "restricted") => {
                bail!(
                    "{}",
                    control["message"].as_str().unwrap_or("huddle refused")
                )
            }
            _ => {}
        }
    }
    bail!("audio socket closed during handshake")
}

fn apply_peers(list: &Value, peers: &mut HashMap<u8, String>) {
    for peer in list.as_array().into_iter().flatten() {
        if let (Some(index), Some(pubkey)) = (peer["peer_index"].as_u64(), peer["pubkey"].as_str())
        {
            if let Ok(index) = u8::try_from(index) {
                peers.insert(index, pubkey.to_owned());
            }
        }
    }
}

fn apply_control(text: &str, peers: &mut HashMap<u8, String>) -> Result<()> {
    let control: Value = serde_json::from_str(text)?;
    match control["type"].as_str() {
        Some("joined") => apply_peers(&control["peers"], peers),
        Some("roster") => {
            peers.clear();
            apply_peers(&control["peers"], peers);
        }
        Some("left") => {
            if let Some(index) = control["peer_index"]
                .as_u64()
                .and_then(|i| u8::try_from(i).ok())
            {
                peers.remove(&index);
            }
        }
        Some("error" | "restricted") => {
            bail!("{}", control["message"].as_str().unwrap_or("huddle ended"))
        }
        _ => {}
    }
    Ok(())
}

fn rand_ssrc() -> u32 {
    let bytes = *Uuid::new_v4().as_bytes();
    u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn start_links_backing_to_parent() {
        let backing = Uuid::new_v4();
        let parent = Uuid::new_v4();
        let events = json!([
            { "kind": 48100, "content": json!({"ephemeral_channel_id": Uuid::new_v4()}).to_string(),
              "tags": [["h", Uuid::new_v4()]] },
            { "kind": 48100, "content": json!({"ephemeral_channel_id": backing}).to_string(),
              "tags": [["h", parent]] },
        ]);
        assert_eq!(
            find_start(&events, backing),
            Some(Huddle { backing, parent })
        );
        assert_eq!(find_start(&events, Uuid::new_v4()), None);
        assert_eq!(
            find_start(&json!([{ "kind": 9, "content": "{}" }]), backing),
            None
        );
    }

    #[test]
    fn frames_match_the_v2_wire() {
        let frame = encode_frame(0x0102, 0x0304_0506, &[7; 30]);
        assert_eq!(&frame[..8], &[1, 2, 3, 4, 5, 6, (-20i8) as u8, 0]);
        assert_eq!(encode_frame(0, 0, &[1])[6..8], [(-127i8) as u8, 1]);
        let mut incoming = vec![9];
        incoming.extend(&frame);
        let (peer, level, opus) = parse_frame(&incoming).unwrap();
        assert_eq!((peer, level, opus.len()), (9, -20, 30));
        assert!(parse_frame(&[1; 9]).is_none());
    }

    #[test]
    fn floor_holds_one_speaker_until_quiet() {
        let mut floor = Floor::default();
        let t = Instant::now();
        assert!(!floor.admit(1, -90, t), "silence takes no floor");
        assert!(floor.admit(1, -20, t));
        assert!(!floor.admit(2, -20, t + FRAME), "another speaker waits");
        assert!(
            floor.admit(1, -90, t + FRAME),
            "holder's quiet frames still flow"
        );
        assert!(
            floor.admit(2, -20, t + FLOOR_HOLD + FRAME),
            "floor frees after the hold"
        );
    }

    #[tokio::test]
    async fn offers_one_gathered_opus_audio_track() {
        let (failed, _) = mpsc::channel(1);
        let (model, _) = mpsc::channel(1);
        let (pc, _, _, offer) = media_endpoint(failed, model).await.unwrap();
        assert!(offer.contains("m=audio"), "{offer}");
        assert!(offer.contains("a=rtpmap:111 opus/48000/2"), "{offer}");
        assert!(offer.contains("a=sendrecv"), "{offer}");
        assert!(
            offer.contains("a=candidate:"),
            "gathered before goose sees it: {offer}"
        );
        assert!(!offer.contains("m=video") && !offer.contains("m=application"));
        pc.close().await.unwrap();
    }

    #[test]
    fn roster_updates_track_peers() {
        let mut peers = HashMap::new();
        apply_control(
            r#"{"type":"joined","pubkey":"a","peers":[{"pubkey":"a","peer_index":3}]}"#,
            &mut peers,
        )
        .unwrap();
        assert_eq!(peers.get(&3).map(String::as_str), Some("a"));
        apply_control(r#"{"type":"left","pubkey":"a","peer_index":3}"#, &mut peers).unwrap();
        assert!(peers.is_empty());
        assert!(apply_control(r#"{"type":"error","message":"room_ended"}"#, &mut peers).is_err());
    }
}
