import { getRelayHttpUrl } from "@/shared/api/tauri";

export interface TranscribeStatus {
  configured: boolean;
  model: string;
}

export interface TranscribeSession {
  clientSecret: string;
  model: string;
}

export async function getTranscribeStatus(): Promise<TranscribeStatus> {
  const baseUrl = await getRelayHttpUrl();
  const response = await fetch(`${baseUrl}/transcribe/status`);
  if (!response.ok) {
    throw new Error(`Transcribe status check failed: ${response.status}`);
  }
  return response.json();
}

export async function createTranscribeSession(): Promise<TranscribeSession> {
  const baseUrl = await getRelayHttpUrl();
  const response = await fetch(`${baseUrl}/transcribe/session`, {
    method: "POST",
    headers: { "Content-Type": "application/json" },
  });
  if (!response.ok) {
    const body = await response.text().catch(() => "");
    throw new Error(
      `Failed to create transcribe session (${response.status}): ${body}`,
    );
  }
  return response.json();
}
