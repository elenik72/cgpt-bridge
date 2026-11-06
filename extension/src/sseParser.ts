// Pure parser for ChatGPT's conversation SSE stream. Used by pageShim.ts; kept
// free of DOM/window references so it can be exercised outside the page.
//
// The stream mixes two encodings:
//   • Legacy full snapshots: `{ message: { id, author, content, recipient }, ... }`.
//   • Delta encoding (`event: delta_encoding` / `data: "v1"`):
//       { p: "", o: "add", v: { message: {...} } }            — new message
//       { p: "/message/content/parts/0", o: "append", v: "Hel" }
//       { v: "lo" }                                           — same p/o as before
//       { p: "", o: "patch", v: [ {p, o, v}, ... ] }          — batch
//     An event without `p`/`o` continues the previous TOP-LEVEL operation:
//     `{ v: { message } }` after an add is another add, `{ v: [ ... ] }`
//     after a patch is another batch (observed 2026-10-23: every message
//     after the first and every batch after the first arrive like that).
//     A bare string `{ v }` continues the last append path, which is not
//     necessarily the answer text: reasoning, tool calls and search steps
//     stream the same way. So text is tracked per message and only
//     assistant `text` messages addressed to everyone count.
//
// Completion (`isComplete`) is taken from the answer itself — an assistant
// `text` message reaching `end_turn: true` — or from
// `type: "message_stream_complete"` / `[DONE]`, never from the end of the
// body: metadata keeps trickling in for seconds after the answer, and the
// page aborts the fetch right after `[DONE]`.

interface StreamMessage {
  id: string | null;
  endTurn: boolean;
  role: string | null;
  contentType: string | null;
  recipient: string | null;
  text: string;
}

export interface SseState {
  messages: StreamMessage[];
  current: StreamMessage | null;
  // Previous top-level operation (what a bare `{ v }` continues).
  lastPath: string | null;
  lastOp: string | null;
  // Previous operation inside a patch batch (what a bare string continues
  // after a batch, and the default for a sub-op without p/o).
  lastSubPath: string | null;
  lastSubOp: string | null;
  complete: boolean;
  // Called with every string that becomes (part of) a message's text, or is
  // appended anywhere: the capture's text pieces (`captureEvent`).
  onText?: ((piece: string) => void) | null;
}

export function newSseState(): SseState {
  return {
    messages: [],
    current: null,
    lastPath: null,
    lastOp: null,
    lastSubPath: null,
    lastSubOp: null,
    complete: false,
  };
}

export function isComplete(state: SseState): boolean {
  return state.complete;
}

const TEXT_PATH = "/message/content/parts/0";

type Json = Record<string, unknown>;

function asObject(v: unknown): Json | null {
  return v !== null && typeof v === "object" && !Array.isArray(v) ? (v as Json) : null;
}

function startMessage(state: SseState, raw: Json): void {
  const author = asObject(raw.author);
  const content = asObject(raw.content);
  const contentType = typeof content?.content_type === "string" ? content.content_type : null;
  const parts = Array.isArray(content?.parts) ? (content.parts as unknown[]) : [];
  const msg: StreamMessage = {
    id: typeof raw.id === "string" ? raw.id : null,
    endTurn: raw.end_turn === true,
    role: typeof author?.role === "string" ? author.role : null,
    contentType,
    recipient: typeof raw.recipient === "string" ? raw.recipient : null,
    text: contentType === "text" ? parts.filter((x) => typeof x === "string").join("") : "",
  };
  if (contentType === "text") {
    for (const x of parts) if (typeof x === "string") state.onText?.(x);
  }
  state.messages.push(msg);
  state.current = msg;
}

function applyOp(state: SseState, path: string, op: string, value: unknown): void {
  if (path === "" && op === "add") {
    const message = asObject(asObject(value)?.message);
    if (message) startMessage(state, message);
    return;
  }
  if (op === "patch" && Array.isArray(value)) {
    for (const sub of value) {
      const o = asObject(sub);
      if (!o) continue;
      const p = typeof o.p === "string" ? o.p : (state.lastSubPath ?? "");
      const so = typeof o.o === "string" ? o.o : (state.lastSubOp ?? "append");
      state.lastSubPath = p;
      state.lastSubOp = so;
      applyOp(state, p, so, o.v);
    }
    return;
  }
  // Whatever streams in pieces is appended: all of it is text for the capture.
  if (op === "append" && typeof value === "string") state.onText?.(value);
  const msg = state.current;
  if (!msg) return;
  if (path === "/message/end_turn" && op === "replace") {
    msg.endTurn = value === true;
    if (msg.endTurn && msg.role === "assistant" && msg.contentType === "text") {
      state.complete = true;
    }
    return;
  }
  if (path !== TEXT_PATH) return;
  if (op === "replace" && typeof value === "string") state.onText?.(value);
  if (op === "append" && typeof value === "string") {
    msg.text += value;
  } else if (op === "replace" && typeof value === "string") {
    msg.text = value;
  } else if (op === "truncate" && typeof value === "number") {
    msg.text = msg.text.slice(0, value);
  }
}

/** Feed the payload of one SSE event (the joined `data:` lines). */
export function sseAccumulate(state: SseState, eventData: string): void {
  if (!eventData) return;
  if (eventData === "[DONE]") {
    state.complete = true;
    return;
  }
  let parsed: unknown;
  try {
    parsed = JSON.parse(eventData);
  } catch {
    return;
  }
  const obj = asObject(parsed);
  if (!obj) return;

  // Legacy snapshot: the whole message is resent each time.
  const message = asObject(obj.message);
  if (message && obj.o === undefined && obj.p === undefined) {
    const prev = state.current;
    if (prev && prev.id !== null && prev.id === message.id) {
      state.messages.pop();
    }
    startMessage(state, message);
    return;
  }

  if (obj.type === "message_stream_complete") {
    state.complete = true;
    return;
  }
  if (!("v" in obj) && !("o" in obj)) return; // other event types (markers, titles)

  let path = typeof obj.p === "string" ? obj.p : (state.lastPath ?? "");
  let op = typeof obj.o === "string" ? obj.o : (state.lastOp ?? "append");
  if (obj.o === undefined && obj.p === undefined && state.lastOp === "patch" && !Array.isArray(obj.v)) {
    // A bare value right after a batch continues the batch's last sub-op.
    path = state.lastSubPath ?? "";
    op = state.lastSubOp ?? "append";
  } else {
    state.lastPath = path;
    state.lastOp = op;
  }
  applyOp(state, path, op, obj.v);
}

/** Text of the newest assistant answer addressed to the user, or "". */
export function answerText(state: SseState): string {
  for (let i = state.messages.length - 1; i >= 0; i--) {
    const m = state.messages[i];
    if (!m) continue;
    if (
      m.role === "assistant" &&
      m.contentType === "text" &&
      (m.recipient === null || m.recipient === "all") &&
      m.text.length > 0
    ) {
      return m.text;
    }
  }
  return "";
}

/**
 * Incremental SSE framer: feed decoded chunks, get back complete event
 * payloads (multiple `data:` lines of one event joined with "\n").
 */
export class SseFramer {
  private buffer = "";
  private data: string[] = [];

  push(chunk: string): string[] {
    this.buffer += chunk;
    const out: string[] = [];
    let nl: number;
    while ((nl = this.buffer.indexOf("\n")) >= 0) {
      const line = this.buffer.slice(0, nl).replace(/\r$/, "");
      this.buffer = this.buffer.slice(nl + 1);
      this.line(line, out);
    }
    return out;
  }

  flush(): string[] {
    const out: string[] = [];
    if (this.buffer.length > 0) {
      this.line(this.buffer.replace(/\r$/, ""), out);
      this.buffer = "";
    }
    this.line("", out);
    return out;
  }

  private line(line: string, out: string[]): void {
    if (line === "") {
      if (this.data.length > 0) out.push(this.data.join("\n"));
      this.data = [];
    } else if (line.startsWith("data:")) {
      this.data.push(line.slice(5).replace(/^ /, ""));
    }
  }
}

// ─── debug capture ─────────────────────────────────────────────────────────
//
// What the shim keeps of an answer stream when the gateway asks for it (its
// `debug_capture_turns`). The stream's text arrives in small pieces, so a
// secret may be split over several events, where no redaction would find
// it. The capture therefore keeps every event with each text piece (any
// string the parser takes as message text or sees appended) replaced by
// `«text OFF+LEN»`, a reference into the pieces joined in stream order, and
// that joined text once at the end, where the gateway's redaction sees it
// whole. Every other string stays as it came (ids, paths, roles, titles:
// whole values the redaction sees as they are). Offsets count UTF-16 units
// of the joined text before redaction. All pieces of a stream share one
// joined text, in arrival order: pieces of two messages streamed at once
// (say, reasoning and answer interleaved) are joined interleaved, and a
// secret could be split by the other message's pieces between its halves.
// Not seen so far: ChatGPT streams one message after another.

export interface StreamCapture {
  events: string;
  pieces: string;
  // Characters kept so far; nothing more once it reaches `max`.
  max: number;
  full: boolean;
}

export function newStreamCapture(max: number): StreamCapture {
  return { events: "", pieces: "", max, full: false };
}

function used(c: StreamCapture): number {
  return c.events.length + c.pieces.length;
}

/** Feed one SSE event to the parser (as `sseAccumulate`) and to the capture. */
export function captureEvent(state: SseState, eventData: string, c: StreamCapture): void {
  const pieces: Array<{ text: string; off: number; taken: boolean }> = [];
  let joined = c.pieces.length;
  state.onText = (piece) => {
    pieces.push({ text: piece, off: joined, taken: false });
    joined += piece.length;
  };
  try {
    sseAccumulate(state, eventData);
  } finally {
    state.onText = null;
  }
  if (c.full) return;
  let line: string;
  let parsed: unknown;
  try {
    parsed = JSON.parse(eventData);
  } catch {
    parsed = undefined;
  }
  if (parsed === undefined) {
    // `[DONE]` and the like; anything else not JSON only by its length.
    line = /^[A-Za-z0-9_\[\]-]{0,32}$/.test(eventData) ? eventData : `«${eventData.length} chars»`;
  } else {
    const swap = (v: unknown): unknown => {
      if (typeof v === "string") {
        const p = pieces.find((x) => !x.taken && x.text === v);
        if (p === undefined) return v;
        p.taken = true;
        return `«text ${p.off}+${v.length}»`;
      }
      if (Array.isArray(v)) return v.map(swap);
      if (v !== null && typeof v === "object") {
        const out: Record<string, unknown> = {};
        for (const [k, x] of Object.entries(v)) out[k] = swap(x);
        return out;
      }
      return v;
    };
    line = JSON.stringify(swap(parsed));
  }
  const add = `data: ${line}\n\n`;
  const text = pieces.map((p) => p.text).join("");
  if (used(c) + add.length + text.length > c.max) {
    c.full = true;
    // The rest of a token split over the last kept event and this one is
    // not kept: drop the kept part too (back to the last whitespace), so
    // no half secret is left where redaction cannot match it.
    c.pieces = c.pieces.replace(/\S+$/, "");
    c.events += ": cgpt-bridge: capture full, the rest of the stream is not kept\n\n";
    return;
  }
  c.events += add;
  c.pieces += text;
}

/** A second stream of the same ask starts here. */
export function captureNextStream(c: StreamCapture, n: number): void {
  if (!c.full) c.events += `: cgpt-bridge: stream ${n}\n\n`;
}

/** The capture as handed to the gateway: the events, then the text pieces. */
export function renderCapture(c: StreamCapture): string {
  return (
    c.events +
    `: cgpt-bridge: text pieces joined (${c.pieces.length} chars); «text OFF+LEN» above points here\n` +
    c.pieces
  );
}
