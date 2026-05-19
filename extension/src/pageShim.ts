// pageShim.ts — runs in chatgpt.com's MAIN JS world via the manifest
// `content_scripts` entry with `"world": "MAIN"`. Bypasses page CSP
// because content scripts execute with extension privilege, not page
// origin. Loaded at `document_start` so we monkey-patch globals before
// the page wires up its own listeners and before any fetch goes out.
//
// What this fixes (in order of how critical each piece turned out to be
// during real testing):
//
//   1. SSE stream hijack. The conversation endpoint streams assistant
//      tokens as Server-Sent Events. When the tab loses focus, ChatGPT's
//      React rendering of those tokens stalls — the audio "audible-tab"
//      trick is no longer enough on recent Chrome versions. We tee the
//      response body and parse the SSE events ourselves in MAIN world,
//      then `postMessage` the accumulated assistant text to the
//      isolated-world content script. The adapter resolves
//      `waitForNewAnswer` as soon as the SSE stream completes, with no
//      dependency on whether React got around to repainting.
//
//   2. `stop_conversation` suppression. The moment a visibilitychange
//      fires, ChatGPT POSTs `/backend-api/stop_conversation` and the
//      server cooperatively tears down the in-flight stream. We
//      short-circuit those POSTs with a synthetic 200 OK so the stream
//      keeps flowing. Side effect: the in-page "Stop" button no longer
//      works while pageShim is loaded — acceptable for a cgpt-bridge
//      session.
//
//   3. Visibility / focus spoof. Defensive. With (2) the stream survives
//      regardless of page state, but pinning `document.hidden` /
//      `visibilityState` / `hasFocus` to "visible+focused" and
//      swallowing `visibilitychange` / `blur` / `pagehide` / `freeze`
//      stops ChatGPT's UI from briefly reacting to the blur (graying
//      buttons, showing "background" hints, etc.). Cheap and harmless.

(() => {
  const W = window as unknown as Record<string, unknown>;
  if (W.__cgptBridgePageShim) return;
  W.__cgptBridgePageShim = true;

  const TAG = "[cgpt-bridge:shim]";
  const debug = (...args: unknown[]) => {
    try {
      // Use info() instead of debug() so the messages show up in the
      // default DevTools console filter — most users don't have the
      // "Verbose" level enabled.
      console.info(TAG, ...args);
    } catch {
      /* console may be locked down */
    }
  };

  // ─── visibility / focus spoof ────────────────────────────────────────
  const def = (obj: object, key: string, getter: () => unknown) => {
    try {
      Object.defineProperty(obj, key, {
        configurable: true,
        enumerable: true,
        get: getter,
      });
    } catch {
      /* non-configurable on some Chromium builds */
    }
  };
  def(document, "hidden", () => false);
  def(document, "visibilityState", () => "visible");
  def(document, "webkitHidden", () => false);
  def(document, "webkitVisibilityState", () => "visible");
  try {
    (document as Document & { hasFocus: () => boolean }).hasFocus = () => true;
  } catch {
    /* readonly on locked-down builds */
  }

  const BLOCKED_EVENTS = [
    "visibilitychange",
    "webkitvisibilitychange",
    "blur",
    "pagehide",
    "freeze",
  ];
  const swallow = (e: Event) => {
    try {
      e.stopImmediatePropagation();
    } catch {
      /* */
    }
    try {
      e.preventDefault();
    } catch {
      /* */
    }
  };
  for (const t of BLOCKED_EVENTS) {
    document.addEventListener(t, swallow, true);
    window.addEventListener(t, swallow, true);
  }

  // ─── helpers ─────────────────────────────────────────────────────────
  const urlOf = (input: RequestInfo | URL): string => {
    if (typeof input === "string") return input;
    if (input instanceof URL) return input.toString();
    if (input instanceof Request) return input.url;
    return String(input);
  };

  // Endpoints that cancel an in-flight chat. We short-circuit them with
  // a fake success response so the SSE stream keeps flowing.
  const STOP_RE = /\/backend-api\/(stop_conversation|conversation\/stop|f\/conversation\/finalize)/;
  const fakeOkResponse = (note: string): Response => {
    const body = JSON.stringify({
      success: true,
      blocked_by: "cgpt-bridge",
      note,
    });
    return new Response(body, {
      status: 200,
      statusText: "OK",
      headers: { "content-type": "application/json" },
    });
  };

  // ─── SSE stream hijack ───────────────────────────────────────────────
  const CONVERSATION_RE = /\/backend-api\/(f\/)?conversation(\?|$|\/?)/;
  const postToContent = (payload: object) => {
    try {
      window.postMessage(
        { __cgptBridge: true, ...payload },
        window.location.origin,
      );
    } catch {
      /* */
    }
  };

  // Pull assistant text out of a single SSE `data:` JSON event.
  // ChatGPT uses several shapes: legacy `{message:{content:{parts:[...]}}}`
  // and newer `{v: "..."} | {p: "/message/content/parts/0", o:"append", v:"..."} | {o:"patch", v:[{p:"...", o:"append", v:"..."}]}`.
  // We aggregate by walking known patches and patches-of-patches.
  const sseAccumulate = (
    state: { text: string },
    eventData: string,
  ): void => {
    if (!eventData || eventData === "[DONE]") return;
    let obj: unknown;
    try {
      obj = JSON.parse(eventData);
    } catch {
      return;
    }
    const visit = (o: unknown) => {
      if (o === null || typeof o !== "object") return;
      const node = o as Record<string, unknown>;
      // Legacy shape: a full message object.
      const msg = node.message as Record<string, unknown> | undefined;
      if (msg && typeof msg === "object") {
        const content = msg.content as Record<string, unknown> | undefined;
        if (content) {
          const parts = content.parts as unknown[] | undefined;
          if (Array.isArray(parts) && parts.length > 0 && typeof parts[0] === "string") {
            // Full replacement (most recent text wins).
            state.text = parts[0] as string;
          }
        }
      }
      // Newer streaming patch shape: { v: "text" } at the top level,
      // applied as append to current text.
      if (typeof node.v === "string" && node.o === undefined && !msg) {
        state.text += node.v;
      }
      // Path-based patch: { p: "/message/content/parts/0", o: "append", v: "text" }
      if (typeof node.p === "string" && typeof node.v === "string") {
        if (node.p.indexOf("/message/content/parts/0") >= 0) {
          if (node.o === "append" || node.o === undefined) {
            state.text += node.v;
          } else if (node.o === "replace") {
            state.text = node.v;
          }
        }
      }
      // Batched patches: { o: "patch", v: [ { p: "...", o: "append", v: "..." }, ... ] }
      if (node.o === "patch" && Array.isArray(node.v)) {
        for (const sub of node.v as unknown[]) visit(sub);
      }
    };
    visit(obj);
  };

  const teeAndParseSse = (response: Response): Response => {
    if (!response.body) return response;
    const ct = response.headers.get("content-type") ?? "";
    if (ct.indexOf("event-stream") < 0) return response;
    const [pageStream, ourStream] = response.body.tee();
    const reader = ourStream.getReader();
    const decoder = new TextDecoder();
    const state = { text: "" };
    let buffer = "";
    let lastReported = "";
    let chunkCount = 0;
    const reportIfChanged = () => {
      if (state.text !== lastReported) {
        lastReported = state.text;
        postToContent({
          kind: "sse-tokens",
          text: state.text,
        });
      }
    };
    const pump = (): Promise<void> =>
      reader.read().then(({ done, value }) => {
        if (done) {
          // Flush any remaining buffer just in case.
          if (buffer.length > 0) {
            for (const line of buffer.split(/\r?\n/)) {
              if (line.startsWith("data:")) sseAccumulate(state, line.slice(5).trim());
            }
            buffer = "";
          }
          reportIfChanged();
          debug("SSE done; total chars=", state.text.length, "chunks=", chunkCount);
          postToContent({ kind: "sse-done", text: state.text });
          return;
        }
        chunkCount++;
        buffer += decoder.decode(value, { stream: true });
        let nl: number;
        while ((nl = buffer.indexOf("\n")) >= 0) {
          const raw = buffer.slice(0, nl).trimEnd();
          buffer = buffer.slice(nl + 1);
          if (raw.startsWith("data:")) {
            sseAccumulate(state, raw.slice(5).trim());
          }
        }
        reportIfChanged();
        return pump();
      }).catch((err) => {
        debug("SSE pump error:", err);
        postToContent({ kind: "sse-error", message: String(err) });
      });
    void pump();
    return new Response(pageStream, {
      status: response.status,
      statusText: response.statusText,
      headers: response.headers,
    });
  };

  // ─── window.fetch wrap ───────────────────────────────────────────────
  const originalFetch = window.fetch.bind(window);
  window.fetch = async function patchedFetch(
    this: typeof globalThis,
    input: RequestInfo | URL,
    init?: RequestInit,
  ): Promise<Response> {
    try {
      const url = urlOf(input);
      if (STOP_RE.test(url)) {
        debug("DROPPED stop_conversation request (URL):", url);
        return fakeOkResponse("stop_conversation suppressed to keep stream alive");
      }
      if (CONVERSATION_RE.test(url)) {
        debug("intercepting SSE conversation:", url);
        const response = await originalFetch(input, init);
        return teeAndParseSse(response);
      }
    } catch (err) {
      debug("fetch shim error (passing through):", err);
    }
    return originalFetch(input, init);
  } as typeof window.fetch;

  // ─── navigator.sendBeacon wrap ───────────────────────────────────────
  // The Page Lifecycle API typically uses sendBeacon to fire telemetry
  // on visibilitychange / pagehide because it survives even when the
  // page is being unloaded. We block stop_conversation through this
  // path too.
  try {
    const origSendBeacon = navigator.sendBeacon.bind(navigator);
    navigator.sendBeacon = function patchedSendBeacon(
      url: string | URL,
      data?: BodyInit | null,
    ): boolean {
      try {
        const u = typeof url === "string" ? url : url.toString();
        if (STOP_RE.test(u)) {
          debug("DROPPED stop_conversation sendBeacon:", u);
          return true;
        }
      } catch (err) {
        debug("sendBeacon shim error (passing through):", err);
      }
      return origSendBeacon(url as string, data);
    } as typeof navigator.sendBeacon;
  } catch (err) {
    debug("sendBeacon wrap install failed:", err);
  }

  // ─── XMLHttpRequest wrap ─────────────────────────────────────────────
  // open() captures the URL, send() short-circuits stop_conversation by
  // synthesizing a 200 readystate flow so consumers don't hang.
  type XhrWithMeta = XMLHttpRequest & { __cgptUrl?: string };
  const xhrOpen = XMLHttpRequest.prototype.open;
  XMLHttpRequest.prototype.open = function patchedOpen(
    this: XhrWithMeta,
    method: string,
    url: string | URL,
    ...rest: unknown[]
  ): void {
    this.__cgptUrl = typeof url === "string" ? url : url.toString();
    // @ts-expect-error — rest spread for variadic optional args
    return xhrOpen.call(this, method, url, ...rest);
  };
  const xhrSend = XMLHttpRequest.prototype.send;
  XMLHttpRequest.prototype.send = function patchedSend(
    this: XhrWithMeta,
    body?: Document | XMLHttpRequestBodyInit | null,
  ): void {
    try {
      const url = this.__cgptUrl ?? "";
      if (STOP_RE.test(url)) {
        debug("DROPPED stop_conversation XHR:", url);
        // Synthesize a 200 OK so callers waiting on readystatechange
        // observe a successful completion. Best-effort; if the consumer
        // inspects the response body it will see a synthetic payload.
        try {
          Object.defineProperty(this, "readyState", { value: 4, configurable: true });
          Object.defineProperty(this, "status", { value: 200, configurable: true });
          Object.defineProperty(this, "statusText", { value: "OK", configurable: true });
          Object.defineProperty(this, "responseText", {
            value: '{"success":true,"blocked_by":"cgpt-bridge"}',
            configurable: true,
          });
          Object.defineProperty(this, "response", {
            value: '{"success":true,"blocked_by":"cgpt-bridge"}',
            configurable: true,
          });
          this.dispatchEvent(new Event("readystatechange"));
          this.dispatchEvent(new Event("load"));
          this.dispatchEvent(new Event("loadend"));
        } catch {
          /* falling through silently is acceptable */
        }
        return;
      }
    } catch (err) {
      debug("xhr shim error (passing through):", err);
    }
    return xhrSend.call(this, body);
  };

  debug("page shim installed");
})();
