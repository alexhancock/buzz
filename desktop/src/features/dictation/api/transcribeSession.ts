import { getRelayHttpUrl, signRelayEvent } from "@/shared/api/tauri";

export interface TranscribeStatus {
  configured: boolean;
  model: string;
}

export interface TranscribeSession {
  sessionId: string;
  model: string;
}

export interface SdpExchangeResponse {
  sdp: string;
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

export async function createTranscribeSession(): Promise<TranscribeSession> {
  const baseUrl = await getRelayHttpUrl();
  const url = `${baseUrl}/transcribe/session`;
  const response = await fetch(url, {
    method: "POST",
    headers: {
      "Content-Type": "application/json",
      Authorization: await nip98AuthHeader(url, "POST"),
    },
  });
  if (!response.ok) {
    const body = await response.text().catch(() => "");
    throw new Error(
      `Failed to create transcribe session (${response.status}): ${body}`,
    );
  }
  return response.json();
}

/**
 * Proxy the WebRTC SDP exchange through the relay. The relay holds the
 * OpenAI client secret server-side — the desktop client never sees it.
 */
export async function proxySdpExchange(
  sessionId: string,
  sdp: string,
): Promise<SdpExchangeResponse> {
  const baseUrl = await getRelayHttpUrl();
  const url = `${baseUrl}/transcribe/sdp`;
  const response = await fetch(url, {
    method: "POST",
    headers: {
      "Content-Type": "application/json",
      Authorization: await nip98AuthHeader(url, "POST"),
    },
    body: JSON.stringify({ sessionId, sdp }),
  });
  if (!response.ok) {
    const body = await response.text().catch(() => "");
    throw new Error(`SDP exchange failed (${response.status}): ${body}`);
  }
  return response.json();
}
