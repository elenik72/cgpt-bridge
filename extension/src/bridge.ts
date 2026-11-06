// Persistent native-messaging bridge.
//
// This module is the production wiring between the cgpt-bridge native host
// (started by Chrome via connectNative) and the rest of the extension.
// `nativeHost.ts` only holds the host's registered name; there is no ephemeral
// ping helper any more (a toolbar click in background.ts talks to the content
// script directly, and only with the `devTestAsk` dev flag set).
//
// Lifecycle:
//   1. On service-worker startup we open a single long-lived port to the
//      native host.
//   2. We send periodic ping frames so MV3 does not evict the SW while we
//      are otherwise idle.
//   3. Incoming requests from the host are routed to a ChatGPT tab (see
//      targetTab.ts for which one):
//        - `ask` without a target (CLI) → the user's active ChatGPT tab;
//        - `ask` with a target (gateway) → a tab of the extension's own
//          window (a pool of tabs, so several gateway sessions run at
//          once), switched to the target chat first; the answer carries the
//          chat's URL;
//        - `stop` (older gateways) → cancel / stop every gateway ask in
//          flight, in every own tab, or with none in flight stop what still
//          generates in the own tab of the last one; user tabs are never
//          touched;
//        - `stop_ask` (Ctrl+C in `cgpt ask`, newer gateways) → stop the one
//          ask it names, in whatever tab it runs, and nothing else;
//        - `status` → state of an own tab, and how many own tabs the
//          extension keeps at most.
//      Asks run one at a time per tab and within `timeout_ms`.
//   4. If the port disconnects (host died, manifest missing, allowed_origins
//      mismatch), we back off and try to reconnect with bounded retries.

import { NATIVE_HOST_NAME } from "./nativeHost";
import {
  InFlight,
  TabQueue,
  isReceivingEndMissing,
  parseHostTarget,
  submitBudgetMs,
  tabLoadTimeoutMessage,
  type ActiveAsk,
} from "./askControl";
import {
  EXTENSION_BUILD,
  askResultFrame,
  capHostFrame,
  errorFrame,
  forOlderHost,
  hostBuildOf,
  isValidProjectName,
  newRequestId,
  parseAttachments,
  statusResultFrame,
  stoppedFrame,
  type AskAttachment,
  type ChatTarget,
  type ExtensionRequest,
  type ExtensionResponse,
  type HostResponseFrame,
  type TestAskRequest,
  type WireErrorCode,
} from "./bridgeProtocol";
import {
  conversationPathOfUrl,
  summarizeDiagnostics,
  type AdapterDiagnostics,
} from "./chatgptAdapter";
import { addressOf, describeTab, injectPageScripts, onChatGpt } from "./reinject";
import { OWN_TABS_MAX, TabChooser } from "./targetTab";

interface HostAskRequest {
  type: "ask";
  id: string;
  text: string;
  timeout_ms: number;
  // Validated by parseHostTarget.
  target?: unknown;
  // Validated by isValidProjectName.
  project?: unknown;
  // Debug aid: only `true` asks for the raw answer stream.
  capture_stream?: unknown;
  // Validated by parseAttachments.
  attachments?: unknown;
}

interface HostStopRequest {
  type: "stop";
  id: string;
}

interface HostStopAskRequest {
  type: "stop_ask";
  id: string;
  // The host's id of the one ask to stop.
  ask_id: unknown;
}

interface HostStatusRequest {
  type: "status";
  id: string;
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
  build?: string;
  echo?: unknown;
  ts_unix_ms?: number;
}

type HostInbound =
  | HostAskRequest
  | HostStopRequest
  | HostStopAskRequest
  | HostStatusRequest
  | HostAskResultMessage
  | HostErrorMessage
  | HostPingMessage;

const KEEPALIVE_INTERVAL_MS = 20_000;
const RECONNECT_BACKOFF_MS = 5_000;
const MAX_RECONNECT_DELAY_MS = 60_000;
const CHATGPT_ORIGIN = "https://chatgpt.com";
// A page load of the own tab (new window, or a full load to reach the
// target chat) may take this long, and never past the ask's deadline.
const PAGE_LOAD_TIMEOUT_MS = 60_000;
// The content script is injected at document_idle, which can come a moment
// after the tab reports "complete".
const CONTENT_READY_RETRIES = 20;
const CONTENT_READY_RETRY_MS = 250;
// How often a loading tab is asked for its status, in case an event is missed.
const LOAD_POLL_MS = 1_000;
// How long a tab that did not load gets to describe its page for the error.
const DIAGNOSE_TIMEOUT_MS = 2_000;
// While the page scripts go into a tab, how often a cancelled ask is looked for.
const CANCEL_POLL_MS = 100;

let port: chrome.runtime.Port | null = null;
let keepaliveTimer: ReturnType<typeof setInterval> | null = null;
let reconnectAttempts = 0;
let started = false;
// The build in the connected host's last `pong`: null when it sent none (an
// older host, which does not know `cancelled` or `tab_busy` either), undefined
// before the first `pong` of this connection (treated as an older host).
let hostBuild: string | null | undefined;

/**
 * The subset of `chrome.tabs` the load wait of an own-tab ask uses, so the
 * send loop is testable with plain fakes (see test/bridgeAsk.test.ts).
 */
export interface AskTabApi {
  tabs: {
    get(tabId: number): Promise<chrome.tabs.Tab>;
    update(tabId: number, props: chrome.tabs.UpdateProperties): Promise<chrome.tabs.Tab | undefined>;
    onUpdated: {
      addListener(cb: (tabId: number, info: chrome.tabs.OnUpdatedInfo) => void): void;
      removeListener(cb: (tabId: number, info: chrome.tabs.OnUpdatedInfo) => void): void;
    };
    onRemoved: {
      addListener(cb: (tabId: number) => void): void;
      removeListener(cb: (tabId: number) => void): void;
    };
  };
}

/**
 * Sends a request to the tab's content script. `mayResend` is asked before a
 * request nobody received is sent again (see `sendToTab`): false once the
 * ask was cancelled. `injecting(true)` when nobody received the request and
 * the page scripts go in, `injecting(false)` right before it is sent again.
 */
export type SendToTab = (
  tabId: number,
  req: ExtensionRequest,
  mayResend?: () => boolean,
  injecting?: (on: boolean) => void,
) => Promise<ExtensionResponse>;

/** What an own-tab ask talks to: the tab events/queries and the content script. */
export interface AskDeps {
  api: AskTabApi;
  sendToTab: SendToTab;
}

const asksByTab = new TabQueue();
const tabs = new TabChooser(chrome, readOwnMark, (tabId) => asksByTab.pending(tabId));
const inFlight = new InFlight();
// The own tab of the latest gateway ask: a `stop` with no gateway ask in
// flight presses Stop there (its answer may still be generating).
let lastOwnTabId: number | null = null;
// Gateway asks that reached their tab and have ended (a timeout, say), by
// the host's id, oldest first: a `stop_ask` for one of them stops what may
// still generate in its tab (see handleStopAskFromHost).
export const ENDED_ASK_MEMORY_MS = 5 * 60_000;
const ENDED_ASK_MEMORY_MAX = 32;
const endedOwnAsks = new Map<string, { tabId: number; endedAt: number }>();
// The host's id of the last ask handed to each tab.
const lastAskInTab = new Map<number, string>();
const liveDeps: AskDeps = { api: chrome, sendToTab };

/**
 * The browser started (`chrome.runtime.onStartup`): a gateway window Chrome
 * restored has new ids and may be taken back by its page's mark (targetTab.ts).
 */
export async function noteBrowserStart(): Promise<void> {
  await tabs.noteBrowserStart().catch((err: unknown) => {
    console.warn("[cgpt-bridge] could not note the browser start for the gateway window:", err);
  });
}

// The own-window mark the page of `tabId` reports; undefined when it does
// not answer. A tab without a content script gets the page scripts first
// (sendToTab), as after an extension reload or update.
async function readOwnMark(tabId: number): Promise<string | null | undefined> {
  const resp = await sendToTab(tabId, { id: newRequestId(), type: "own_mark" });
  return resp.ok && resp.kind === "own_mark" ? resp.mark : undefined;
}

/**
 * Start the persistent bridge. Idempotent: safe to call from multiple SW
 * boot paths (action listener, alarm wake-up, etc).
 */
export function startBridge(): void {
  if (started) return;
  started = true;
  console.log(`[cgpt-bridge] bridge starting (build ${EXTENSION_BUILD})`);
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
  // `connectNative` succeeds even when the host dies at once, so only the
  // host's own answer (the `pong` to the ping below) resets the backoff.

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

  sendPing("hello");
  startKeepalive();
}

function sendPing(prefix: string): void {
  if (port === null) return;
  try {
    port.postMessage({ type: "ping", id: `${prefix}_${Date.now().toString(36)}` });
  } catch (err) {
    console.warn(`[cgpt-bridge] ${prefix} ping postMessage failed:`, err);
  }
}

function teardown(): void {
  if (keepaliveTimer !== null) {
    clearInterval(keepaliveTimer);
    keepaliveTimer = null;
  }
  port = null;
  // The next connection may be another host binary.
  hostBuild = undefined;
}

function startKeepalive(): void {
  if (keepaliveTimer !== null) clearInterval(keepaliveTimer);
  keepaliveTimer = setInterval(() => sendPing("ka"), KEEPALIVE_INTERVAL_MS);
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
    case "stop":
      void handleStopFromHost(msg);
      return;
    case "stop_ask":
      void handleStopAskFromHost(msg);
      return;
    case "status":
      void handleStatusFromHost(msg);
      return;
    case "pong":
      // The host is alive and answering: the next failure starts the
      // reconnect delay from the first step again.
      reconnectAttempts = 0;
      noteHostBuild(hostBuildOf(msg));
      return;
    case "ask_result":
    case "error":
      // Responses to our own outbound traffic. Nothing to do; the host has
      // already correlated them.
      return;
    default: {
      const unknown = (msg as { type?: string }).type;
      console.warn(`[cgpt-bridge] ignoring host message of type ${String(unknown)}`);
    }
  }
}

// One line when the host's build is first seen or changes, a warning when it
// is not this extension's.
function noteHostBuild(build: string | null): void {
  if (build === hostBuild) return;
  hostBuild = build;
  if (build === EXTENSION_BUILD) {
    console.info(`[cgpt-bridge] native host build ${build}`);
  } else {
    console.warn(
      `[cgpt-bridge] native host build ${build ?? "(none)"} differs from the extension build ${EXTENSION_BUILD}`,
    );
  }
}

// Posts the frame (or the error that replaces an oversize one) and returns
// what was meant to go out and whether it was handed to the port, for the
// outcome line.
function respond(
  id: string,
  payload: HostResponseFrame,
): { frame: HostResponseFrame; delivered: boolean } {
  if (port === null) {
    console.warn(`[cgpt-bridge] cannot respond to ${id}: native port closed`);
    return { frame: payload, delivered: false };
  }
  const frame = capHostFrame(payload);
  if (frame !== payload) {
    console.warn(
      frame.type === "error"
        ? `[cgpt-bridge] response for ${id} is over the host frame limit, sending an error`
        : `[cgpt-bridge] response for ${id} is over the host frame limit with its stream capture: sent without it`,
    );
  }
  try {
    // The outcome line keeps `cancelled` and `tab_busy` even when an older
    // host gets them the old way (forOlderHost).
    port.postMessage(typeof hostBuild === "string" ? frame : forOlderHost(frame));
  } catch (err) {
    console.error(`[cgpt-bridge] failed to post response for ${id}:`, err);
    return { frame, delivered: false };
  }
  return { frame, delivered: true };
}

function respondError(id: string, code: WireErrorCode, message: string): void {
  respond(id, errorFrame(id, code, message));
}

// The one line an ask leaves in the service worker console: the host's id (the
// gateway log and the host carry the same one; the page gets it as the prefix of
// its request id), the tab (`-` when none was chosen), what the ask ended in and
// how long it took. `undelivered` means the native port was closed and the host
// got nothing. Ids, codes and numbers only: never the prompt or the answer.
function logAskOutcome(
  id: string,
  tabId: number | null,
  sent: { frame: HostResponseFrame; delivered: boolean },
  startedAt: number,
): void {
  const { frame, delivered } = sent;
  // An answer the closed port could not carry is not an `ok`.
  const outcome = !delivered ? "undelivered" : frame.type === "error" ? frame.code : "ok";
  console.info(
    `[cgpt-bridge] ask ${id} tab=${tabId ?? "-"} outcome=${outcome} ${Date.now() - startedAt} ms`,
  );
}

async function handleAskFromHost(ask: HostAskRequest): Promise<void> {
  // The host gives up shortly after `timeout_ms`; everything below — the
  // wait for an earlier ask in the same tab, page loads, the turn itself —
  // has to fit before this.
  const startedAt = Date.now();
  const deadline = startedAt + ask.timeout_ms;
  const end = (tabId: number | null, frame: HostResponseFrame): void => {
    // A gateway ask's outcome tells the next choice whether its own tab
    // may still be generating.
    if (tabId !== null && ask.target !== undefined && ask.target !== null) {
      tabs.noteOutcome(tabId, frame.type === "error" ? frame.code : null);
    }
    logAskOutcome(ask.id, tabId, respond(ask.id, frame), startedAt);
  };
  const parsed = parseHostTarget(ask.target);
  if (!parsed.ok) {
    end(null, errorFrame(ask.id, "bad_request", parsed.message));
    return;
  }
  const target = parsed.target;
  if (ask.project !== undefined && !isValidProjectName(ask.project)) {
    end(null, errorFrame(ask.id, "bad_request", "project is not a valid ChatGPT project name"));
    return;
  }
  const files = parseAttachments(ask.attachments);
  if (!files.ok) {
    end(null, errorFrame(ask.id, "bad_request", files.message));
    return;
  }

  // Registered before its tab is chosen, so a `stop` reaches it while the
  // tab is being found or opened (that can take seconds) and while it is
  // queued behind another ask: either way it then ends `cancelled` and
  // nothing reaches the tab.
  const active = inFlight.start({
    tabId: null,
    hostId: ask.id,
    own: target !== undefined,
    // Starts with the host's id: the page's records and this worker's carry
    // the id the gateway log has.
    contentRequestId: `${ask.id}.c`,
  });
  try {
    let tabId: number;
    try {
      tabId = await tabs.tabForAsk(target);
    } catch (err) {
      const message = err instanceof Error ? err.message : String(err);
      end(null, errorFrame(ask.id, "tab_unavailable", message));
      return;
    }
    active.tabId = tabId;
    if (target !== undefined) lastOwnTabId = tabId;
    if (active.cancelled) {
      if (target !== undefined) tabs.unreserve(tabId);
      end(tabId, tabErrorFrame(ask.id, "cancelled", "cancelled by stop"));
      return;
    }
    // One ask per tab at a time: a second one would type into the chat the
    // first one is using, and the stream tee / shim are per page. `run`
    // counts the ask in the tab's queue at once, so the own tab's
    // reservation can go right after the call.
    const running = asksByTab.run(tabId, deadline, () =>
      runAsk(ask, active, tabId, target, deadline, files.attachments),
    );
    if (target !== undefined) tabs.unreserve(tabId);
    const queued = await running;
    if (!queued.ran) {
      end(
        tabId,
        errorFrame(
          ask.id,
          "timeout",
          "timed out waiting for the previous request in the same ChatGPT tab; nothing was sent",
        ),
      );
      return;
    }
    end(tabId, queued.value);
  } finally {
    inFlight.finish(active);
    if (active.own && active.handedToTab && active.tabId !== null) {
      endedOwnAsks.delete(ask.id);
      endedOwnAsks.set(ask.id, { tabId: active.tabId, endedAt: Date.now() });
      for (const id of endedOwnAsks.keys()) {
        if (endedOwnAsks.size <= ENDED_ASK_MEMORY_MAX) break;
        endedOwnAsks.delete(id);
      }
    }
  }
}

async function runAsk(
  ask: HostAskRequest,
  active: ActiveAsk,
  tabId: number,
  target: ChatTarget | undefined,
  deadline: number,
  attachments: AskAttachment[],
): Promise<HostResponseFrame> {
  // A `stop` came while this ask waited for its turn: nothing is sent.
  if (active.cancelled) return tabErrorFrame(ask.id, "cancelled", "cancelled by stop");
  const ownMark = target === undefined ? null : await tabs.ownMarkFor(tabId);
  if (active.cancelled) return tabErrorFrame(ask.id, "cancelled", "cancelled by stop");
  const req: TestAskRequest = {
    id: active.contentRequestId,
    type: "test.ask",
    text: ask.text,
    timeoutMs: ask.timeout_ms,
    deadlineMs: deadline,
    ...(target === undefined ? {} : { target }),
    ...(isValidProjectName(ask.project) ? { project: ask.project } : {}),
    // The page keeps it: an extension reload or a browser restart can then
    // tell this window from the user's tabs.
    ...(ownMark === null ? {} : { ownMark }),
    ...(ask.capture_stream === true ? { captureStream: true } : {}),
    ...(attachments.length === 0 ? {} : { attachments }),
  };
  active.handedToTab = true;
  lastAskInTab.set(tabId, ask.id);
  const resp =
    target === undefined
      ? await sendToTab(tabId, req, () => !active.cancelled, (on) => (active.scriptsGoingIn = on))
      : await askOwnTab(liveDeps, tabId, req, active);
  if (!resp.ok) return tabErrorFrame(ask.id, resp.errorCode, resp.message);
  if (resp.kind !== "test.ask") {
    return errorFrame(ask.id, "internal", `unexpected tab response kind ${resp.kind}`);
  }
  if (target === undefined) await tabs.rememberLegacyTarget(tabId);
  return askResultFrame(ask.id, resp.text, resp.conversationUrl, resp.warning ?? null, resp.diagnostics ?? null);
}

function tabErrorFrame(id: string, contentCode: string, message: string): HostResponseFrame {
  return errorFrame(id, mapTabErrorCode(contentCode), message);
}

// Targeted ask in the own tab. When the content script cannot reach the
// target in-page it answers "navigate"; the tab is then loaded at that URL
// (once) and the same request is sent again, marked `reloaded`.
export async function askOwnTab(
  deps: AskDeps,
  tabId: number,
  req: TestAskRequest,
  active: ActiveAsk,
): Promise<ExtensionResponse> {
  const deadline = req.deadlineMs ?? Date.now() + req.timeoutMs;
  const fail = (errorCode: string, message: string): ExtensionResponse => ({
    id: req.id,
    ok: false,
    errorCode,
    message,
  });

  let resp = await sendWhenLoaded(deps, tabId, req, active, deadline);
  if (!resp.ok || resp.kind !== "navigate") return resp;

  if (active.cancelled) return fail("cancelled", "cancelled by stop");
  if (deadline - Date.now() < submitBudgetMs(req.timeoutMs)) {
    return fail("answer_timeout", "no time left to load the target chat; nothing was sent");
  }
  const url = resp.url;
  const loaded = await waitForTabLoad(deps, tabId, deadline, url, () =>
    deps.api.tabs.update(tabId, { url }),
  );
  if (!loaded.ok) return fail(loaded.code, loaded.message);

  resp = await sendWhenLoaded(deps, tabId, { ...req, reloaded: true }, active, deadline);
  if (resp.ok && resp.kind === "navigate") {
    return fail("target_not_reached", "the target chat did not open after a page load");
  }
  return resp;
}

// Send once the tab has loaded, retrying only while no content script is
// listening yet (document_idle can come a moment after "complete").
export async function sendWhenLoaded(
  deps: AskDeps,
  tabId: number,
  req: ExtensionRequest,
  active: ActiveAsk,
  deadline: number,
): Promise<ExtensionResponse> {
  const fail = (errorCode: string, message: string): ExtensionResponse => ({
    id: req.id,
    ok: false,
    errorCode,
    message,
  });
  const loaded = await waitForTabLoad(deps, tabId, deadline, "the page the tab was loading");
  if (!loaded.ok) return fail(loaded.code, loaded.message);
  for (let attempt = 0; ; attempt++) {
    if (active.cancelled) return fail("cancelled", "cancelled by stop");
    const resp = await deps.sendToTab(
      tabId,
      req,
      () => !active.cancelled,
      (on) => (active.scriptsGoingIn = on),
    );
    if (
      resp.ok ||
      resp.errorCode !== "content_script_not_ready" ||
      attempt >= CONTENT_READY_RETRIES ||
      Date.now() + CONTENT_READY_RETRY_MS >= deadline
    ) {
      return resp;
    }
    await new Promise((r) => setTimeout(r, CONTENT_READY_RETRY_MS));
  }
}

type LoadResult = { ok: true } | { ok: false; code: string; message: string };

// Resolves when the tab has finished loading. With `navigate` (e.g.
// `chrome.tabs.update`) the listeners are in place before it runs and a
// fresh loading → complete cycle is required — the current "complete"
// belongs to the page that is going away. Bounded by the deadline.
// `waitingFor` (the URL being loaded, or what else) goes into the error.
function waitForTabLoad(
  deps: AskDeps,
  tabId: number,
  deadline: number,
  waitingFor: string,
  navigate?: () => Promise<unknown>,
): Promise<LoadResult> {
  const timeoutMs = Math.min(PAGE_LOAD_TIMEOUT_MS, Math.max(0, deadline - Date.now()));
  const deadlineBound = timeoutMs < PAGE_LOAD_TIMEOUT_MS;
  const closed: LoadResult = {
    ok: false,
    code: "content_script_not_ready",
    message: "the ChatGPT tab was closed",
  };
  return new Promise((resolve) => {
    let sawLoading = navigate === undefined;
    let settled = false;
    const finish = (result: LoadResult): void => {
      if (settled) return;
      settled = true;
      clearTimeout(timer);
      clearInterval(poll);
      deps.api.tabs.onUpdated.removeListener(onUpdated);
      deps.api.tabs.onRemoved.removeListener(onRemoved);
      resolve(result);
    };
    const onUpdated = (id: number, info: chrome.tabs.OnUpdatedInfo): void => {
      if (id !== tabId) return;
      if (info.status === "loading") sawLoading = true;
      else if (info.status === "complete" && sawLoading) finish({ ok: true });
    };
    const onRemoved = (id: number): void => {
      if (id === tabId) finish(closed);
    };
    const timer = setTimeout(() => {
      if (deadlineBound) {
        finish({ ok: false, code: "answer_timeout", message: "deadline passed while the ChatGPT tab was loading" });
        return;
      }
      void describeLoadTimeout(deps, tabId, waitingFor).then((message) =>
        finish({ ok: false, code: "content_script_not_ready", message }),
      );
    }, timeoutMs);
    // Live (26.10.2026): a background tab sat at status "complete" for 60 s
    // without the "complete" event reaching us. Ask the tab itself as well.
    const poll = setInterval(() => {
      if (!sawLoading) return;
      deps.api.tabs.get(tabId).then(
        (tab) => {
          if (tab.status === "complete") finish({ ok: true });
        },
        () => finish(closed),
      );
    }, LOAD_POLL_MS);
    deps.api.tabs.onUpdated.addListener(onUpdated);
    deps.api.tabs.onRemoved.addListener(onRemoved);
    if (navigate !== undefined) {
      navigate().catch((err: unknown) =>
        finish({
          ok: false,
          code: "content_script_not_ready",
          message: `could not load the target chat: ${err instanceof Error ? err.message : String(err)}`,
        }),
      );
    } else {
      deps.api.tabs.get(tabId).then(
        (tab) => {
          if (tab.status === "complete") finish({ ok: true });
        },
        () => finish(closed),
      );
    }
  });
}

// What the tab and its page showed when a load did not finish in time.
async function describeLoadTimeout(
  deps: AskDeps,
  tabId: number,
  waitingFor: string,
): Promise<string> {
  const tab = await deps.api.tabs.get(tabId).catch(() => null);
  const resp = await Promise.race([
    deps.sendToTab(tabId, { id: newRequestId(), type: "diagnose" }),
    new Promise<null>((resolve) => setTimeout(() => resolve(null), DIAGNOSE_TIMEOUT_MS)),
  ]);
  const page =
    resp !== null && resp.ok && resp.kind === "diagnose"
      ? summarizeDiagnostics(resp.diagnostics as AdapterDiagnostics)
      : null;
  return tabLoadTimeoutMessage({
    timeoutMs: PAGE_LOAD_TIMEOUT_MS,
    waitingFor,
    tab: tab === null ? null : { status: tab.status, url: tab.url, pendingUrl: tab.pendingUrl },
    page,
  });
}

// The plain `stop` of a gateway that does not know `stop_ask`: it runs one
// turn at a time, so a stop is about every gateway ask in flight.
async function handleStopFromHost(req: HostStopRequest): Promise<void> {
  // Only own tabs are touched (only the gateway sends stops); never press
  // Stop in a user tab.
  const byTab = inFlight.ownByTab();
  // No more (re-)sends from the background, for the asks running, those
  // queued behind them and those whose tab is still being chosen.
  for (const own of inFlight.ownAsks()) own.cancelled = true;
  let presses: Array<{ tabId: number; askId: string | undefined }>;
  if (byTab.size > 0) {
    presses = [...byTab].map(([tabId, a]) => ({ tabId, askId: a.contentRequestId }));
  } else {
    // No ask in a tab — but the last one may have ended (timeout) while its
    // answer still generates in its own tab.
    const pool = await tabs.ownTabs().catch((err: unknown) => {
      console.warn("[cgpt-bridge] stop: could not read the gateway tab record:", err);
      return [];
    });
    const tab = pool.find((t) => t.id === lastOwnTabId) ?? pool[0];
    if (tab === undefined) {
      respond(req.id, stoppedFrame(req.id, false));
      return;
    }
    presses = [{ tabId: tab.id, askId: undefined }];
  }
  const outcomes = await Promise.all(presses.map((p) => pressStop(p.tabId, p.askId)));
  const stopped = outcomes.filter((o): o is StopPressed => o.ok);
  const failed = outcomes.find((o): o is StopFailed => !o.ok);
  if (stopped.length === 0 && failed !== undefined) {
    respondError(req.id, failed.code, failed.message);
    return;
  }
  respond(
    req.id,
    stoppedFrame(
      req.id,
      stopped.some((o) => o.wasGenerating),
      stopped.length > 0 && stopped.every((o) => o.otherChat),
    ),
  );
}

// A stop of the one ask it names (Ctrl+C in `cgpt ask`), in its tab — a
// user tab too: the user asked to stop their own ask. With no such ask in
// flight nothing is touched (`stopped` with `was_generating: false`); never
// the own tab instead.
async function handleStopAskFromHost(req: HostStopAskRequest): Promise<void> {
  const named = typeof req.ask_id === "string" ? inFlight.named(req.ask_id) : null;
  if (named === null) {
    await stopEndedOwnAsk(req);
    return;
  }
  // No more (re-)sends for it; the asks queued behind it go on.
  named.cancelled = true;
  if (named.tabId === null || !named.handedToTab) {
    // Its tab is still being chosen, or it waits behind another ask: it ends
    // `cancelled` and its prompt never reaches the tab.
    respond(req.id, stoppedFrame(req.id, false, false, true));
    return;
  }
  await stopInTab(req, named.tabId, named.contentRequestId, named);
}

// A `stop_ask` for a gateway ask that has ended — the gateway gave up on
// its turn (timeout, page failure, its client left) after the ask itself
// timed out here — while its answer may still be generating in its pool
// tab. Stop is pressed there only while that ask is the last one the tab
// got and nothing runs or waits in it, the tab is still a pool tab on
// chatgpt.com and the ask ended less than ENDED_ASK_MEMORY_MS ago. The
// page has closed that ask, so the stop names none: it presses Stop only if
// the page's last gateway turn, this ask's, still generates. A CLI ask that
// is not in flight is never looked up: nothing is touched for it.
async function stopEndedOwnAsk(req: HostStopAskRequest): Promise<void> {
  const nothing = (): void => {
    respond(req.id, stoppedFrame(req.id, false));
  };
  const askId = typeof req.ask_id === "string" ? req.ask_id : null;
  const ended = askId === null ? undefined : endedOwnAsks.get(askId);
  if (ended === undefined || Date.now() - ended.endedAt >= ENDED_ASK_MEMORY_MS) {
    nothing();
    return;
  }
  const { tabId } = ended;
  const pool = await tabs.ownTabs().catch(() => []);
  // A tab just chosen for a new ask counts as busy too (its reservation),
  // so the Stop never reaches a turn that is about to start there.
  const busy =
    asksByTab.pending(tabId) > 0 ||
    tabs.reservations(tabId) > 0 ||
    inFlight.ownByTab().has(tabId);
  if (!pool.some((t) => t.id === tabId) || busy || lastAskInTab.get(tabId) !== askId) {
    nothing();
    return;
  }
  await stopInTab(req, tabId, undefined);
}

interface StopPressed {
  ok: true;
  wasGenerating: boolean;
  otherChat: boolean;
  notSent: boolean;
  // No content script took the stop (the page scripts may be going in).
  noReceiver?: true;
}
interface StopFailed {
  ok: false;
  code: WireErrorCode;
  message: string;
}

// Have the content script in `tabId` press Stop — for the ask `askId` only
// when given (it leaves any other ask alone).
async function pressStop(tabId: number, askId: string | undefined): Promise<StopPressed | StopFailed> {
  const resp = await sendToTab(tabId, {
    id: newRequestId(),
    type: "stop",
    ...(askId === undefined ? {} : { askId }),
  });
  if (resp.ok && resp.kind === "stop") {
    return {
      ok: true,
      wasGenerating: resp.wasGenerating,
      otherChat: resp.otherChat === true,
      notSent: resp.notSent === true,
    };
  }
  if (
    !resp.ok &&
    (resp.errorCode === "content_script_not_ready" || resp.errorCode === "page_reloaded")
  ) {
    // Between page loads: the prompt has not reached this page.
    return {
      ok: true,
      wasGenerating: false,
      otherChat: false,
      notSent: false,
      ...(resp.errorCode === "content_script_not_ready" ? { noReceiver: true as const } : {}),
    };
  }
  if (!resp.ok) return { ok: false, code: mapTabErrorCode(resp.errorCode), message: resp.message };
  return { ok: false, code: "internal", message: `unexpected tab response kind ${resp.kind}` };
}

// `pressStop` for a `stop_ask`, and the answer to the host. `named`: the
// ask in flight it is for. When no content script took the stop while that
// ask's own request had not been received either (the page scripts were
// going in for it), the ask ends `cancelled` without a resend: its prompt
// was not sent, and the host is told so (`not_sent`).
async function stopInTab(
  req: HostStopAskRequest,
  tabId: number,
  askId: string | undefined,
  named?: ActiveAsk,
): Promise<void> {
  const outcome = await pressStop(tabId, askId);
  if (!outcome.ok) {
    respondError(req.id, outcome.code, outcome.message);
    return;
  }
  const notSent = outcome.notSent || (outcome.noReceiver === true && named?.scriptsGoingIn === true);
  respond(req.id, stoppedFrame(req.id, outcome.wasGenerating, outcome.otherChat, notSent));
}

async function handleStatusFromHost(req: HostStatusRequest): Promise<void> {
  const pool = await tabs.ownTabs().catch((err: unknown) => {
    console.warn("[cgpt-bridge] status: could not read the gateway tab record:", err);
    return [];
  });
  // Any own tab tells whether the window is signed in; one with no ask in it
  // answers at once.
  const own = pool.find((t) => asksByTab.pending(t.id) === 0) ?? pool[0];
  if (own === undefined) {
    respond(
      req.id,
      statusResultFrame(req.id, {
        tabOpen: false,
        loggedIn: false,
        conversationUrl: null,
        ownTabs: OWN_TABS_MAX,
        attachments: true,
      }),
    );
    return;
  }
  const resp = await sendToTab(own.id, { id: newRequestId(), type: "status" });
  const fromPage = resp.ok && resp.kind === "status" ? resp : null;
  const tabPath = conversationPathOfUrl(own.url);
  respond(
    req.id,
    statusResultFrame(req.id, {
      tabOpen: true,
      // No answer from the page (still loading): not usable yet.
      loggedIn: fromPage?.loggedIn ?? false,
      conversationUrl: fromPage
        ? fromPage.conversationUrl
        : tabPath === null
          ? null
          : CHATGPT_ORIGIN + tabPath,
      ownTabs: OWN_TABS_MAX,
      attachments: true,
    }),
  );
}

function mapTabErrorCode(contentCode: string): WireErrorCode {
  // Translate adapter error codes (defined in errors.ts) and the
  // background's own codes to the wire-level ErrorCode enum the host/CLI
  // understand.
  switch (contentCode) {
    case "unsupported_page":
      return "tab_unavailable";
    case "composer_not_found":
    case "send_button_not_found":
    case "stop_button_not_found":
    case "answer_not_detected":
    case "target_not_reached":
    case "target_mismatch":
    case "chat_url_missing":
    case "page_reloaded":
    // The files did not upload, or an earlier ask's files could not be
    // removed; nothing was sent (the message says so).
    case "attachment_upload_failed":
    case "attachments_left_over":
      return "dom_failure";
    // Refused before anything was typed: an earlier answer still generating,
    // or another ask running in the tab.
    case "previous_answer_running":
    case "ask_running":
      return "tab_busy";
    case "answer_timeout":
      return "timeout";
    case "rate_limited":
      return "rate_limited";
    // Stop in the page, or a `stop` request (message "cancelled by stop").
    case "cancelled":
      return "cancelled";
    case "bad_target":
      return "bad_request";
    case "content_script_not_ready":
    case "bad_request":
    // The own window is signed out: the user has to sign in there.
    case "not_logged_in":
      return "tab_unavailable";
    default:
      return "internal";
  }
}

// Sends `req` to the content script of `tabId`. A tab where none listens —
// open since before the extension was installed, updated or reloaded, or a
// page between loads — gets the page scripts injected (reinject.ts) and the
// same request once more. Only "no receiver" is re-sent: nobody took the
// request, so nothing of it reached the chat. Never into a tab that has left
// chatgpt.com, and never once `mayResend` says no (a `stop` came for the ask
// meanwhile, and was told the prompt has not reached the page).
async function sendToTab(
  tabId: number,
  req: ExtensionRequest,
  mayResend: () => boolean = () => true,
  injecting: (on: boolean) => void = () => {},
): Promise<ExtensionResponse> {
  const first = await sendOnce(tabId, req);
  if (!("noReceiver" in first)) return first;
  // A content script injected now would know of no ask to stop.
  if (req.type === "stop") return notReady(req, first.noReceiver);
  const before = await chrome.tabs.get(tabId).catch(() => null);
  if (before === null || !onChatGpt(before.url)) {
    const where =
      before === null
        ? "the tab is gone"
        : `the tab left chatgpt.com, it is at ${addressOf(before.url ?? "(no address)")}`;
    return notReady(req, `${first.noReceiver} (${where}; nothing was injected)`);
  }
  injecting(true);
  const notInjected = await injectUnlessCancelled(tabId, mayResend);
  if (notInjected === CANCELLED || !mayResend()) {
    return { id: req.id, ok: false, errorCode: "cancelled", message: "cancelled by stop" };
  }
  injecting(false);
  const again = notInjected === null ? await sendOnce(tabId, req) : first;
  if (!("noReceiver" in again)) return again;
  const tab = await chrome.tabs.get(tabId).catch(() => null);
  return notReady(
    req,
    `${again.noReceiver} (no cgpt-bridge content script answers in ${describeTab(tab)}; ` +
      (notInjected === null
        ? "it was injected again and still does not; "
        : `it could not be injected: ${notInjected}; `) +
      "reload that tab)",
  );
}

const CANCELLED = Symbol("cancelled");

// `injectPageScripts`, or CANCELLED as soon as `mayResend` says no: a
// stopped ask ends at once, not when the page has taken the scripts (they
// still go in; that is harmless, they run once per page).
async function injectUnlessCancelled(
  tabId: number,
  mayResend: () => boolean,
): Promise<string | null | typeof CANCELLED> {
  let poll: ReturnType<typeof setInterval> | undefined;
  const cancelled = new Promise<typeof CANCELLED>((resolve) => {
    poll = setInterval(() => {
      if (!mayResend()) resolve(CANCELLED);
    }, CANCEL_POLL_MS);
  });
  try {
    return await Promise.race([injectPageScripts(chrome, tabId), cancelled]);
  } finally {
    clearInterval(poll);
  }
}

function notReady(req: ExtensionRequest, message: string): ExtensionResponse {
  return { id: req.id, ok: false, errorCode: "content_script_not_ready", message };
}

// One send. `noReceiver` (Chrome's message) when no content script listens.
function sendOnce(
  tabId: number,
  req: ExtensionRequest,
): Promise<ExtensionResponse | { noReceiver: string }> {
  return new Promise((resolve) => {
    chrome.tabs.sendMessage(tabId, req, (resp: ExtensionResponse | undefined) => {
      const lastError = chrome.runtime.lastError;
      if (lastError) {
        const message = lastError.message ?? "lastError";
        resolve(
          isReceivingEndMissing(message)
            ? { noReceiver: message }
            : {
                // The content script took the request and the page went
                // away: never re-send, the prompt may already be in the chat.
                id: req.id,
                ok: false,
                errorCode: "page_reloaded",
                message: `the page reloaded during the turn (${message})`,
              },
        );
        return;
      }
      if (!resp) {
        resolve({
          id: req.id,
          ok: false,
          errorCode: "page_reloaded",
          message: "the page reloaded during the turn (content script returned no response)",
        });
        return;
      }
      resolve(resp);
    });
  });
}
