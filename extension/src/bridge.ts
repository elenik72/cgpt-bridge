// Persistent native-messaging bridge.
//
// This module is the production wiring between the cgpt-bridge native host
// (started by Chrome via connectNative) and the rest of the extension. It is
// distinct from `nativeHost.ts`, which provides the ephemeral `pingNative()`
// helper used for SW-console diagnostics.
//
// Lifecycle:
//   1. On service-worker startup we open a single long-lived port to the
//      native host.
//   2. We send periodic ping frames so MV3 does not evict the SW while we
//      are otherwise idle.
//   3. Incoming `ask` requests from the host are routed to the active
//      ChatGPT tab via the same content-script handler used by the toolbar
//      button.
//   4. If the port disconnects (host died, manifest missing, allowed_origins
//      mismatch), we back off and try to reconnect with bounded retries.

import { NATIVE_HOST_NAME } from "./nativeHost";
import { newRequestId, type ExtensionRequest, type ExtensionResponse } from "./bridgeProtocol";

interface HostAskRequest {
  type: "ask";
  id: string;
  text: string;
  timeout_ms: number;
}

interface HostAskResultMessage {
  type: "ask_result";
  id: string;
  text: string;
}

interface HostErrorMessage {
  type: "error";
  id?: string;
  code: string;
  message: string;
}

interface HostPingMessage {
  type: "pong";
  id?: string;
  host_version?: string;
  echo?: unknown;
  ts_unix_ms?: number;
}

type HostInbound = HostAskRequest | HostAskResultMessage | HostErrorMessage | HostPingMessage;

const KEEPALIVE_INTERVAL_MS = 20_000;
const RECONNECT_BACKOFF_MS = 5_000;
const MAX_RECONNECT_DELAY_MS = 60_000;
const CHATGPT_URL_PREFIX = "https://chatgpt.com/";

let port: chrome.runtime.Port | null = null;
let keepaliveTimer: ReturnType<typeof setInterval> | null = null;
let reconnectAttempts = 0;
let started = false;

/**
 * Start the persistent bridge. Idempotent: safe to call from multiple SW
 * boot paths (action listener, alarm wake-up, etc).
 */
export function startBridge(): void {
  if (started) return;
  started = true;
  console.log("[cgpt-bridge] bridge starting");
  openPort();
}

function openPort(): void {
  if (port !== null) return;

  let next: chrome.runtime.Port;
  try {
    next = chrome.runtime.connectNative(NATIVE_HOST_NAME);
  } catch (err) {
    console.error(
      "[cgpt-bridge] connectNative threw — is the native host installed?",
      err,
    );
    scheduleReconnect();
    return;
  }
  port = next;
  console.log("[cgpt-bridge] native port opened");
  reconnectAttempts = 0;

  next.onMessage.addListener(handleHostMessage);
  next.onDisconnect.addListener(() => {
    const lastError = chrome.runtime.lastError;
    console.warn(
      "[cgpt-bridge] native port disconnected:",
      lastError?.message ?? "(no lastError)",
    );
    teardown();
    scheduleReconnect();
  });

  startKeepalive();
}

function teardown(): void {
  if (keepaliveTimer !== null) {
    clearInterval(keepaliveTimer);
    keepaliveTimer = null;
  }
  port = null;
}

function startKeepalive(): void {
  if (keepaliveTimer !== null) clearInterval(keepaliveTimer);
  keepaliveTimer = setInterval(() => {
    if (port === null) return;
    try {
      port.postMessage({ type: "ping", id: `ka_${Date.now().toString(36)}` });
    } catch (err) {
      console.warn("[cgpt-bridge] keepalive postMessage failed:", err);
    }
  }, KEEPALIVE_INTERVAL_MS);
}

function scheduleReconnect(): void {
  reconnectAttempts += 1;
  const delay = Math.min(
    RECONNECT_BACKOFF_MS * reconnectAttempts,
    MAX_RECONNECT_DELAY_MS,
  );
  console.log(
    `[cgpt-bridge] scheduling reconnect attempt #${reconnectAttempts} in ${delay} ms`,
  );
  setTimeout(openPort, delay);
}

function handleHostMessage(raw: unknown): void {
  const msg = raw as HostInbound | undefined;
  if (!msg || typeof msg !== "object" || typeof (msg as { type?: unknown }).type !== "string") {
    console.warn("[cgpt-bridge] ignoring unrecognized host message:", raw);
    return;
  }

  switch (msg.type) {
    case "ask":
      void handleAskFromHost(msg);
      return;
    case "ask_result":
    case "error":
    case "pong":
      // Responses to our own outbound traffic (keepalive pings, etc).
      // Nothing to do; the host has already correlated them.
      return;
    default: {
      const unknown = (msg as { type?: string }).type;
      console.warn(`[cgpt-bridge] ignoring host message of type ${String(unknown)}`);
    }
  }
}

async function handleAskFromHost(ask: HostAskRequest): Promise<void> {
  const respond = (payload: object) => {
    if (port === null) {
      console.warn(
        `[cgpt-bridge] cannot respond to ask ${ask.id}: native port closed`,
      );
      return;
    }
    try {
      port.postMessage(payload);
    } catch (err) {
      console.error(
        `[cgpt-bridge] failed to post response for ask ${ask.id}:`,
        err,
      );
    }
  };

  let tab: chrome.tabs.Tab | undefined;
  try {
    tab = await findActiveChatGptTab();
  } catch (err) {
    respond({
      type: "error",
      id: ask.id,
      code: "tab_unavailable",
      message: err instanceof Error ? err.message : String(err),
    });
    return;
  }

  if (typeof tab.id !== "number") {
    respond({
      type: "error",
      id: ask.id,
      code: "tab_unavailable",
      message: "active chatgpt.com tab has no id",
    });
    return;
  }

  const tabRequest: ExtensionRequest = {
    id: newRequestId(),
    type: "test.ask",
    text: ask.text,
    timeoutMs: ask.timeout_ms,
  };

  const tabResponse = await sendToTab(tab.id, tabRequest);
  if (tabResponse.ok) {
    if (tabResponse.kind === "test.ask") {
      respond({ type: "ask_result", id: ask.id, text: tabResponse.text });
    } else {
      respond({
        type: "error",
        id: ask.id,
        code: "internal",
        message: `unexpected tab response kind ${tabResponse.kind}`,
      });
    }
  } else {
    respond({
      type: "error",
      id: ask.id,
      code: mapTabErrorCode(tabResponse.errorCode),
      message: tabResponse.message,
    });
  }
}

function mapTabErrorCode(contentCode: string): string {
  // Translate adapter error codes (defined in errors.ts) to the wire-level
  // ErrorCode enum the host/CLI understand.
  switch (contentCode) {
    case "unsupported_page":
      return "tab_unavailable";
    case "composer_not_found":
    case "send_button_not_found":
      return "dom_failure";
    case "answer_timeout":
      return "timeout";
    case "content_script_not_ready":
    case "bad_request":
      return "tab_unavailable";
    default:
      return "internal";
  }
}

async function findActiveChatGptTab(): Promise<chrome.tabs.Tab> {
  // Preference order:
  //   1. The active chatgpt.com tab in the last-focused window (matches the
  //      user's mental model: "the chat I am looking at").
  //   2. The active chatgpt.com tab in any other window.
  //   3. Any chatgpt.com tab anywhere — we keep working when the user has
  //      tabbed away to the terminal. Chrome throttles background tabs'
  //      setTimeout/setInterval so polling slows down, but mutation events
  //      still fire and the content script does eventually return.
  const queries: chrome.tabs.QueryInfo[] = [
    { active: true, lastFocusedWindow: true, url: `${CHATGPT_URL_PREFIX}*` },
    { active: true, url: `${CHATGPT_URL_PREFIX}*` },
    { url: `${CHATGPT_URL_PREFIX}*` },
  ];
  for (const q of queries) {
    const tabs = await chrome.tabs.query(q);
    const tab = tabs.find(
      (t) =>
        typeof t.id === "number" &&
        typeof t.url === "string" &&
        t.url.startsWith(CHATGPT_URL_PREFIX),
    );
    if (tab) {
      return tab;
    }
  }
  throw new Error(
    "no https://chatgpt.com/ tab is open — open chatgpt.com in any Chrome window",
  );
}

function sendToTab(tabId: number, req: ExtensionRequest): Promise<ExtensionResponse> {
  return new Promise((resolve) => {
    chrome.tabs.sendMessage(tabId, req, (resp: ExtensionResponse | undefined) => {
      const lastError = chrome.runtime.lastError;
      if (lastError) {
        resolve({
          id: req.id,
          ok: false,
          errorCode: "content_script_not_ready",
          message:
            `${lastError.message ?? "lastError"} (try reloading the ChatGPT tab ` +
            `after installing or reloading the extension)`,
        });
        return;
      }
      if (!resp) {
        resolve({
          id: req.id,
          ok: false,
          errorCode: "content_script_not_ready",
          message: "content script returned no response",
        });
        return;
      }
      resolve(resp);
    });
  });
}
