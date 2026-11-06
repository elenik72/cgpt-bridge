// Where a gateway ask should land (host wire form, passed through unchanged).
// Untargeted asks (the CLI) have no target and use the user's active tab.
export type ChatTarget =
  | { kind: "conversation"; url: string }
  | { kind: "new_chat" };

export interface TestAskRequest {
  id: string;
  type: "test.ask";
  text: string;
  timeoutMs: number;
  target?: ChatTarget;
  // Epoch ms by which the caller needs the answer; every wait in the page is
  // cut to it. Absent (toolbar test) → now + timeoutMs.
  deadlineMs?: number;
  // The page was just loaded to reach `target`; failing to show it now is
  // final instead of asking for another load.
  reloaded?: boolean;
  // The ChatGPT project a new chat goes into (isValidProjectName); a chat
  // that exists is continued where it is.
  project?: string;
  // The own window's mark (targetTab.ts), on gateway asks into the own tab
  // only: the page keeps it in its sessionStorage.
  ownMark?: string;
  // Debug aid (the gateway's `debug_capture_turns`): return the answer
  // stream (sseParser.ts captureEvent) in `diagnostics.streamCapture`.
  captureStream?: boolean;
  // Files the page puts into the composer before it types the text (images
  // from OpenCode, sent by the gateway); checked by parseAttachments.
  attachments?: AskAttachment[];
}

// One file of an ask: the host's `{name, mime, data_base64}`.
export interface AskAttachment {
  name: string;
  mime: string;
  dataBase64: string;
}

// The types the gateway sends (it reads them from the bytes).
export const ATTACHMENT_MIME_TYPES = ["image/png", "image/jpeg", "image/gif", "image/webp"] as const;
// Most files in one ask (the gateway's MAX_IMAGES).
export const MAX_ATTACHMENTS = 10;
const ATTACHMENT_NAME = /^[A-Za-z0-9._-]{1,64}$/;
const BASE64 = /^(?:[A-Za-z0-9+/]{4})*(?:[A-Za-z0-9+/]{2}==|[A-Za-z0-9+/]{3}=)?$/;

export type ParsedAttachments =
  | { ok: true; attachments: AskAttachment[] }
  | { ok: false; message: string };

// An ask's `attachments` as the host sends them (`data_base64`) or as the
// background hands them to the page (`dataBase64`): absent is none; anything
// but a list of at most MAX_ATTACHMENTS files with a plain name, one of
// ATTACHMENT_MIME_TYPES and standard base64 is refused (the frame's size is
// bounded by the host already).
export function parseAttachments(value: unknown): ParsedAttachments {
  if (value === undefined || value === null) return { ok: true, attachments: [] };
  if (!Array.isArray(value)) return { ok: false, message: "attachments must be a list" };
  if (value.length > MAX_ATTACHMENTS) {
    return { ok: false, message: `more than ${MAX_ATTACHMENTS} attachments` };
  }
  const out: AskAttachment[] = [];
  for (const [i, item] of value.entries()) {
    const a = (typeof item === "object" && item !== null ? item : {}) as Record<string, unknown>;
    const data = a.data_base64 ?? a.dataBase64;
    if (typeof a.name !== "string" || !ATTACHMENT_NAME.test(a.name)) {
      return { ok: false, message: `attachment ${i + 1}: bad name` };
    }
    if (typeof a.mime !== "string" || !(ATTACHMENT_MIME_TYPES as readonly string[]).includes(a.mime)) {
      return { ok: false, message: `attachment ${i + 1}: type ${String(a.mime)} is not an image the bridge sends` };
    }
    if (typeof data !== "string" || data === "" || !BASE64.test(data)) {
      return { ok: false, message: `attachment ${i + 1}: data is not base64` };
    }
    out.push({ name: a.name, mime: a.mime, dataBase64: data });
  }
  return { ok: true, attachments: out };
}

export interface DiagnoseRequest {
  id: string;
  type: "diagnose";
}

export interface StopRequest {
  id: string;
  type: "stop";
  // `id` of the ask to stop; a different (or finished) ask is left alone.
  // Absent: the background has no ask in flight in this (own) tab — stop
  // whatever is still generating there.
  askId?: string;
}

export interface StatusRequest {
  id: string;
  type: "status";
}

// Which own-window mark the page carries, if any (read-only).
export interface OwnMarkRequest {
  id: string;
  type: "own_mark";
}

export type ExtensionRequest =
  | TestAskRequest
  | DiagnoseRequest
  | StopRequest
  | StatusRequest
  | OwnMarkRequest;

export interface TestAskOkResponse {
  id: string;
  ok: true;
  kind: "test.ask";
  text: string;
  // Saved conversation the answer came from (`https://chatgpt.com/c/<id>`,
  // or `/g/<project>-<slug>/c/<id>` in a ChatGPT project), null when the
  // page never left the temporary URL.
  conversationUrl: string | null;
  // The new chat could not be put into the project asked for and went to a
  // plain new chat (shown to the user by the CLI, logged by the gateway).
  warning?: string;
  // What the page noticed about the answer; absent when there is nothing.
  diagnostics?: AskDiagnostics;
}

// The answer from the stream next to the answer the page shows, both
// normalized alike (chatgptAdapter.ts `compareStreamAndPage`): lengths in
// characters and whether they differ.
export interface StreamCheck {
  streamChars: number;
  pageChars: number;
  mismatch: boolean;
}

export interface AskDiagnostics {
  // Only when the stream and the page differ: a report, the answer `text`
  // stays the one the ask took.
  streamCheck?: StreamCheck;
  // The raw answer stream (capped), only when the ask set `captureStream`.
  streamCapture?: string;
}

// The target chat cannot be reached in-page: the background loads `url` in
// the tab and sends the request again (with `reloaded: true`).
export interface NavigateResponse {
  id: string;
  ok: true;
  kind: "navigate";
  url: string;
}

export interface DiagnoseOkResponse {
  id: string;
  ok: true;
  kind: "diagnose";
  diagnostics: unknown;
}

export interface StopOkResponse {
  id: string;
  ok: true;
  kind: "stop";
  // The ask had been submitted and was still running when Stop came.
  wasGenerating: boolean;
  // A CLI ask's stop pressed nothing: the page shows another chat.
  otherChat?: true;
  // The ask had not sent its prompt: it aborts and never will.
  notSent?: true;
}

export interface StatusOkResponse {
  id: string;
  ok: true;
  kind: "status";
  loggedIn: boolean;
  conversationUrl: string | null;
}

export interface OwnMarkOkResponse {
  id: string;
  ok: true;
  kind: "own_mark";
  // Null when the page has none (or its sessionStorage cannot be read).
  mark: string | null;
}

export type OkResponse =
  | TestAskOkResponse
  | NavigateResponse
  | DiagnoseOkResponse
  | StopOkResponse
  | StatusOkResponse
  | OwnMarkOkResponse;

export interface ErrorResponse {
  id: string;
  ok: false;
  errorCode: string;
  message: string;
}

export type ExtensionResponse = OkResponse | ErrorResponse;

// Version of the messages between content.ts and pageShim.ts (switch on/off,
// acknowledgement with `epoch`, stream reports). The shim marks the page
// with it and puts it in every acknowledgement as `v`; an acknowledgement
// without `v` carries an epoch from a shim of version 1. A tab keeps the
// shim it loaded across an extension update, so raise this with any change
// to those messages: content.ts then refuses an older shim instead of
// misreading it.
export const PAGE_SHIM_PROTOCOL = 1;

// Frames the background sends back to the native host. Their shape is the
// Rust `BridgeResponse` (protocol/src/message.rs); the fixtures in
// protocol/tests/fixtures/ are checked by both sides.

// Set by the build (vite.config.ts) from CGPT_BUILD, the same value the Rust
// binaries take; `dev` without it (and in the tests).
declare const __CGPT_BUILD__: string | undefined;
export const EXTENSION_BUILD: string =
  typeof __CGPT_BUILD__ === "string" && __CGPT_BUILD__ !== "" ? __CGPT_BUILD__ : "dev";

// The Rust `ErrorCode` enum, wire form.
export const WIRE_ERROR_CODES = [
  "bad_request",
  "oversize_frame",
  "extension_unavailable",
  "tab_unavailable",
  "dom_failure",
  "timeout",
  "rate_limited",
  "cancelled",
  "tab_busy",
  "internal",
] as const;

export type WireErrorCode = (typeof WIRE_ERROR_CODES)[number];

export interface AskResultFrame {
  type: "ask_result";
  id: string;
  text: string;
  conversation_url?: string;
  warning?: string;
  // Rust `AskDiagnostics` (protocol/src/message.rs).
  diagnostics?: WireDiagnostics;
}

export interface WireDiagnostics {
  stream_check?: { stream_chars: number; page_chars: number; mismatch: boolean };
  stream_capture?: string;
  // The capture was asked for but left out: the frame would be too big.
  stream_capture_dropped?: true;
}

export interface ErrorFrame {
  type: "error";
  id: string;
  code: WireErrorCode;
  message: string;
}

export interface StoppedFrame {
  type: "stopped";
  id: string;
  was_generating: boolean;
  // `stop_ask` only: nothing was pressed, the tab shows another chat.
  other_chat?: true;
  // `stop_ask` only: the ask was cancelled before its prompt was sent;
  // nothing of it reaches ChatGPT.
  not_sent?: true;
}

export interface StatusResultFrame {
  type: "status_result";
  id: string;
  extension: true;
  tab_open: boolean;
  logged_in: boolean;
  conversation_url: string | null;
  build: string;
  // How many own tabs (gateway turns at once) the extension keeps at most.
  // Older extensions leave it out; the gateway then runs one turn at a time.
  own_tabs?: number;
  // This extension puts an ask's `attachments` into the composer. Older ones
  // leave it out, and the gateway then sends no files.
  attachments?: true;
}

export type HostResponseFrame = AskResultFrame | ErrorFrame | StoppedFrame | StatusResultFrame;

// `conversation_url` is left out when the page never left the temporary URL,
// `warning` when there is none, `diagnostics` when there is nothing in it.
export function askResultFrame(
  id: string,
  text: string,
  conversationUrl: string | null,
  warning: string | null = null,
  diagnostics: AskDiagnostics | null = null,
): AskResultFrame {
  const wire: WireDiagnostics = {
    ...(diagnostics?.streamCheck === undefined
      ? {}
      : {
          stream_check: {
            stream_chars: diagnostics.streamCheck.streamChars,
            page_chars: diagnostics.streamCheck.pageChars,
            mismatch: diagnostics.streamCheck.mismatch,
          },
        }),
    ...(diagnostics?.streamCapture === undefined ? {} : { stream_capture: diagnostics.streamCapture }),
  };
  return {
    type: "ask_result",
    id,
    text,
    ...(conversationUrl === null ? {} : { conversation_url: conversationUrl }),
    ...(warning === null ? {} : { warning }),
    ...(Object.keys(wire).length === 0 ? {} : { diagnostics: wire }),
  };
}

// Rust `MAX_PROJECT_NAME_CHARS` (protocol/src/message.rs).
export const MAX_PROJECT_NAME_CHARS = 100;

// Rust `check_project_name`: a name that is not empty, at most 100 characters
// (code points), with no control character and no whitespace at either end.
export function isValidProjectName(name: unknown): name is string {
  return (
    typeof name === "string" &&
    name.trim() !== "" &&
    name.trim() === name &&
    [...name].length <= MAX_PROJECT_NAME_CHARS &&
    !/\p{Cc}/u.test(name)
  );
}

export function errorFrame(id: string, code: WireErrorCode, message: string): ErrorFrame {
  return { type: "error", id, code, message };
}

export function stoppedFrame(
  id: string,
  wasGenerating: boolean,
  otherChat = false,
  notSent = false,
): StoppedFrame {
  return {
    type: "stopped",
    id,
    was_generating: wasGenerating,
    ...(otherChat ? { other_chat: true } : {}),
    ...(notSent ? { not_sent: true } : {}),
  };
}

// State of the own tab; `conversation_url` is sent as null, not left out.
// `build` is this extension's.
export function statusResultFrame(
  id: string,
  status: {
    tabOpen: boolean;
    loggedIn: boolean;
    conversationUrl: string | null;
    ownTabs?: number;
    attachments?: boolean;
  },
  build: string = EXTENSION_BUILD,
): StatusResultFrame {
  return {
    type: "status_result",
    id,
    extension: true,
    tab_open: status.tabOpen,
    logged_in: status.loggedIn,
    conversation_url: status.conversationUrl,
    build,
    ...(status.ownTabs === undefined ? {} : { own_tabs: status.ownTabs }),
    ...(status.attachments === true ? { attachments: true as const } : {}),
  };
}

// A build id as scripts/build-id.sh makes them.
const BUILD_ID = /^[A-Za-z0-9._-]{1,64}$/;

// The host's build from its `pong`; null from a host built before builds were
// reported, which also does not know the `cancelled` and `tab_busy` codes.
// Anything but a build id counts as none: it is logged, and it opens that gate.
export function hostBuildOf(pong: { build?: unknown }): string | null {
  return typeof pong.build === "string" && BUILD_ID.test(pong.build) ? pong.build : null;
}

// What goes to a host that does not know `cancelled` or `tab_busy`: it would
// fail the whole frame, so they are sent the old way, with the same message.
// A stop is `internal` (the gateway still recognizes the exact text,
// protocol/tests/fixtures/stop_messages.txt; remove together with that
// fallback), a busy tab `dom_failure`.
export function forOlderHost(frame: HostResponseFrame): HostResponseFrame {
  if (frame.type !== "error") return frame;
  if (frame.code === "cancelled") return { ...frame, code: "internal" };
  if (frame.code === "tab_busy") return { ...frame, code: "dom_failure" };
  return frame;
}

// The host drops a frame from the extension over this size (Rust
// `MAX_FRAME_BYTES`, protocol/src/framing.rs).
export const MAX_HOST_FRAME_BYTES = 1024 * 1024;

// Kept free under the limit as a precaution. Chrome posts the value through
// V8's JSON stringify, so `hostFrameSize` should match the bytes sent exactly;
// the margin only covers a serializer that differs. The real guarantee is the
// host: it drops a frame over its limit and fails that ask, never the host.
export const HOST_FRAME_RESERVE_BYTES = 16 * 1024;

// Bytes of the frame's JSON as it goes to the host.
export function hostFrameSize(frame: HostResponseFrame): number {
  return new TextEncoder().encode(JSON.stringify(frame)).length;
}

// A frame the host would drop for its size becomes an error for the same id,
// so only that ask fails and the caller is told why. The `oversize_frame` code
// is the one the CLI and the gateway already report as a failed ask; the
// message does not carry any of the answer.
//
// A debug capture never fails an ask and is never cut (its joined text
// pieces sit at its end, and a cut there could split a secret before the
// gateway redacts it): a frame too big with it goes without it, marked
// `stream_capture_dropped`.
export function capHostFrame(frame: HostResponseFrame): HostResponseFrame {
  const budget = MAX_HOST_FRAME_BYTES - HOST_FRAME_RESERVE_BYTES;
  const size = hostFrameSize(frame);
  if (size <= budget) return frame;
  if (frame.type === "ask_result" && frame.diagnostics?.stream_capture !== undefined) {
    const { stream_capture: _dropped, ...rest } = frame.diagnostics;
    const without: AskResultFrame = { ...frame, diagnostics: { ...rest, stream_capture_dropped: true } };
    if (hostFrameSize(without) <= budget) return without;
  }
  return errorFrame(frame.id, "oversize_frame", `ответ ${size} байт больше лимита моста`);
}

export function newRequestId(): string {
  const rand = Math.random().toString(36).slice(2, 10);
  return `req_${Date.now().toString(36)}_${rand}`;
}
