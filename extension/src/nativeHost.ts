// Chrome Native Messaging client. Talks to the Rust host binary registered
// under the name "com.cgpt_bridge.host". Chrome spawns the host on demand
// when we call chrome.runtime.connectNative(...) and tears it down when the
// port disconnects.
//
// M4 scope: a single ping/pong helper. Later milestones will use the same
// connection for the cgpt ask/agent request flow.

export const NATIVE_HOST_NAME = "com.cgpt_bridge.host";

export interface PingResult {
  ok: true;
  durationMs: number;
  response: unknown;
}

export interface PingFailure {
  ok: false;
  durationMs: number;
  errorCode: string;
  message: string;
}

export type PingOutcome = PingResult | PingFailure;

interface PingOptions {
  timeoutMs?: number;
  payload?: unknown;
}

/**
 * Opens a fresh native-messaging port, sends one ping, awaits exactly one
 * response, then disconnects. The port is intentionally short-lived in M4;
 * long-lived sessions are a later milestone.
 */
export function pingNative(options: PingOptions = {}): Promise<PingOutcome> {
  const timeoutMs = options.timeoutMs ?? 5000;
  const requestId = `ping_${Date.now().toString(36)}_${Math.random()
    .toString(36)
    .slice(2, 8)}`;
  const startedAt = Date.now();

  return new Promise<PingOutcome>((resolve) => {
    let settled = false;
    let port: chrome.runtime.Port | null = null;
    let timer: ReturnType<typeof setTimeout> | null = null;

    const settle = (outcome: PingOutcome) => {
      if (settled) return;
      settled = true;
      if (timer !== null) clearTimeout(timer);
      try {
        port?.disconnect();
      } catch {
        // Best-effort: disconnect may throw if already disconnected.
      }
      resolve(outcome);
    };

    try {
      port = chrome.runtime.connectNative(NATIVE_HOST_NAME);
    } catch (err) {
      settle({
        ok: false,
        durationMs: Date.now() - startedAt,
        errorCode: "connect_native_threw",
        message: err instanceof Error ? err.message : String(err),
      });
      return;
    }

    port.onMessage.addListener((msg: unknown) => {
      settle({
        ok: true,
        durationMs: Date.now() - startedAt,
        response: msg,
      });
    });

    port.onDisconnect.addListener(() => {
      const lastError = chrome.runtime.lastError;
      settle({
        ok: false,
        durationMs: Date.now() - startedAt,
        errorCode: "disconnected",
        message:
          lastError?.message ??
          "Native host disconnected before sending a response. " +
            "Verify the manifest is installed and the extension id in " +
            "allowed_origins matches this extension.",
      });
    });

    timer = setTimeout(() => {
      settle({
        ok: false,
        durationMs: Date.now() - startedAt,
        errorCode: "timeout",
        message: `Native host did not respond within ${timeoutMs}ms.`,
      });
    }, timeoutMs);

    try {
      port.postMessage({
        type: "ping",
        id: requestId,
        payload: options.payload ?? { from: "cgpt-bridge-extension" },
      });
    } catch (err) {
      settle({
        ok: false,
        durationMs: Date.now() - startedAt,
        errorCode: "post_message_threw",
        message: err instanceof Error ? err.message : String(err),
      });
    }
  });
}
