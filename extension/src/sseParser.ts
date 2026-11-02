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
