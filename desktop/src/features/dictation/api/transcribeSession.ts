import { getRelayHttpUrl, signRelayEvent } from "@/shared/api/tauri";

export interface TranscribeStatus {
  configured: boolean;
  model: string;
}

export interface TranscribeConnectResponse {
  sdp: string;
  model: string;
}

/** NIP-98 event kind for HTTP request authorization. */
const NIP98_KIND = 27235;

/**
 * Build a NIP-98 `Authorization: Nostr <base64>` header for an HTTP request.
 *
 * The relay verifies the signed event's `u` tag against its own
 * host-derived expected URL, so `url` must be the exact absolute URL being
 * fetched (scheme + host + path). The `method` tag must match the request.
 */
async function nip98AuthHeader(url: string, method: string): Promise<string> {
  const nonce = crypto.randomUUID();
  const event = await signRelayEvent({
    kind: NIP98_KIND,
    content: "",
    tags: [
      ["u", url],
      ["method", method],
      ["nonce", nonce],
    ],
  });
  const json = JSON.stringify(event);
  // btoa needs a binary string; encode UTF-8 first so non-ASCII survives.
  const base64 = btoa(String.fromCharCode(...new TextEncoder().encode(json)));
  return `Nostr ${base64}`;
}

export async function getTranscribeStatus(): Promise<TranscribeStatus> {
  const baseUrl = await getRelayHttpUrl();
  const url = `${baseUrl}/transcribe/status`;
  const response = await fetch(url, {
    headers: { Authorization: await nip98AuthHeader(url, "GET") },
  });
  if (!response.ok) {
    throw new Error(`Transcribe status check failed: ${response.status}`);
  }
  return response.json();
}

/**
 * Mint an OpenAI Realtime session and complete the WebRTC SDP exchange in a
 * single relay round-trip. The relay holds the OpenAI bearer token server-side
 * — the client never sees it. This also works correctly across multiple relay
 * replicas since no server-side session state is needed between requests.
 */
export async function transcribeConnect(
  sdp: string,
): Promise<TranscribeConnectResponse> {
  const baseUrl = await getRelayHttpUrl();
  const url = `${baseUrl}/transcribe/connect`;
  const response = await fetch(url, {
    method: "POST",
    headers: {
      "Content-Type": "application/json",
      Authorization: await nip98AuthHeader(url, "POST"),
    },
    body: JSON.stringify({ sdp }),
  });
  if (!response.ok) {
    const body = await response.text().catch(() => "");
    throw new Error(`Transcribe connect failed (${response.status}): ${body}`);
  }
  return response.json();
}
