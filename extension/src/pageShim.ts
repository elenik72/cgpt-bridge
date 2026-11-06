// pageShim.ts — runs in chatgpt.com's MAIN JS world via the manifest
// `content_scripts` entry with `"world": "MAIN"`. Bypasses page CSP
// because content scripts execute with extension privilege, not page
// origin. Loaded at `document_start` so we monkey-patch globals before
// the page wires up its own listeners and before any fetch goes out.
//
// Everything below is DORMANT unless a bridge request is in flight. The
// content script switches it on right before it submits a prompt and off
// when the reply is in (see `setPageShimActive` in chatgptAdapter.ts):
//
//   { __cgptBridgeControl: true, active: true, ttlMs }   — on, auto-off after ttlMs
//   { __cgptBridgeControl: true, active: false }         — off
//   ← { __cgptBridgeAck: true, active, epoch, v }        — acknowledgement
//
// An "on" with `capture: true` (a debug aid the gateway asks for) also keeps
// this epoch's answer streams, at most CAPTURE_MAX_CHARS, in the form
// sseParser.ts `captureEvent` gives them (every event, its text pieces
// joined at the end so that a split secret can be redacted); the content
// script takes it once the answer is in:
//
//   { __cgptBridgeCaptureTake: true, epoch }              — take it
//   ← { __cgptBridgeCapture: true, epoch, raw }           — raw: null if none
//
// Its acknowledgement then carries `capture: true`. A shim without capture
// leaves that out and ignores both messages: the content script then skips
// the take (no wait) and logs that the page script is too old for capture.
// No PAGE_SHIM_PROTOCOL bump: an older shim misreads nothing, and a bump
// would make every tab that kept its shim across an update read all its
// answers from the DOM alone until reloaded, for a debug aid.
//
// `v` is PAGE_SHIM_PROTOCOL (bridgeProtocol.ts), the version of these
// messages; the page is marked with it too. A tab keeps its shim across an
// extension update, and content.ts uses only a shim of its own version.
//
// Every "on" starts a new `epoch`. A stream belongs to the epoch the shim
// was in when the page fetched it, and every report about it carries that
// epoch: a stream outliving its request (the DOM settled first) keeps
// reporting after the next "on", and the next request must not take it for
// its own answer.
//
// While active (in order of how critical each piece turned out to be
// during real testing):
//
//   1. SSE stream hijack. The conversation endpoint streams assistant
//      tokens as Server-Sent Events. When the tab loses focus, ChatGPT's
//      React rendering of those tokens stalls. We tee the response body,
//      parse the events ourselves (sseParser.ts) and `postMessage` the
//      answer text to the isolated-world content script, which resolves
//      `waitForNewAnswer` as soon as the stream completes. An HTTP 429 on
//      that endpoint is reported as `sse-error` with `status: 429`
//      (rate limit).
//
//   2. `stop_conversation` suppression. The moment a visibilitychange
//      fires, ChatGPT POSTs `/backend-api/stop_conversation` and the
//      server cooperatively tears down the in-flight stream. We
//      short-circuit those POSTs with a synthetic 200 OK so the stream
//      keeps flowing. Because this is scoped to a bridge request, the
//      in-page "Stop" button keeps working the rest of the time.
//
//   3. Visibility / focus spoof. Pinning `document.hidden` /
//      `visibilityState` / `hasFocus` to "visible+focused" and swallowing
//      `visibilitychange` / `blur` / `pagehide` / `freeze` stops ChatGPT's
//      UI from reacting to the blur. When the request ends the real values
//      come back and, if the tab is hidden, the page gets one synthetic
//      `visibilitychange` so it re-syncs.

import { PAGE_SHIM_PROTOCOL } from "./bridgeProtocol";
import {
  SseFramer,
  answerText,
  captureEvent,
  captureNextStream,
  isComplete,
  newSseState,
  newStreamCapture,
  renderCapture,
  sseAccumulate,
  type StreamCapture,
} from "./sseParser";

(() => {
  const W = window as unknown as Record<string, unknown>;
  if (W.__cgptBridgePageShim) return;
  W.__cgptBridgePageShim = PAGE_SHIM_PROTOCOL;

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

  // ─── activation ──────────────────────────────────────────────────────
  let active = false;
  let epoch = 0;
  // The answer streams of epoch `capture.epoch` (debug aid), or null.
  const CAPTURE_MAX_CHARS = 512 * 1024;
  let capture: { epoch: number; kept: StreamCapture; streams: number } | null = null;
  let expiry: ReturnType<typeof setTimeout> | null = null;
  const setActive = (on: boolean, ttlMs: number): void => {
    if (expiry !== null) {
      clearTimeout(expiry);
      expiry = null;
    }
    const was = active;
    active = on;
    if (on) {
      // Safety net: if the content script never switches us off (tab
      // navigated, extension reloaded mid-request), go dormant anyway.
      expiry = setTimeout(() => setActive(false, 0), Math.max(ttlMs, 1_000));
    }
    if (was !== on) debug(on ? "active" : "dormant");
    if (was && !on && realHidden()) {
      try {
        document.dispatchEvent(new Event("visibilitychange"));
      } catch {
        /* */
      }
    }
  };
  window.addEventListener("message", (ev: MessageEvent) => {
    if (ev.source !== window) return;
    const d = ev.data as Record<string, unknown> | null;
    if (d && typeof d === "object" && d.__cgptBridgeCaptureTake === true) {
      const mine = capture !== null && capture.epoch === d.epoch ? capture : null;
      if (mine !== null) capture = null;
      try {
        window.postMessage(
          { __cgptBridgeCapture: true, epoch: d.epoch, raw: mine === null ? null : renderCapture(mine.kept) },
          window.location.origin,
        );
      } catch {
        /* */
      }
      return;
    }
    if (!d || typeof d !== "object" || d.__cgptBridgeControl !== true) return;
    const ttl = typeof d.ttlMs === "number" ? d.ttlMs : 0;
    if (d.active === true) {
      epoch++;
      capture = d.capture === true ? { epoch, kept: newStreamCapture(CAPTURE_MAX_CHARS), streams: 0 } : null;
    }
    setActive(d.active === true, ttl);
    try {
      window.postMessage(
        {
          __cgptBridgeAck: true,
          active,
          epoch,
          v: PAGE_SHIM_PROTOCOL,
          // Says this shim keeps the capture asked for (a shim from before
          // captures leaves it out).
          ...(active && capture !== null ? { capture: true } : {}),
        },
        window.location.origin,
      );
    } catch {
      /* */
    }
  });

  // ─── visibility / focus spoof ────────────────────────────────────────
  const protoGetter = (key: string): (() => unknown) | null => {
    const desc = Object.getOwnPropertyDescriptor(Document.prototype, key);
    return desc && typeof desc.get === "function" ? desc.get : null;
  };
  const realHiddenGetter = protoGetter("hidden");
  const realHidden = (): boolean =>
    realHiddenGetter ? Boolean(realHiddenGetter.call(document)) : false;

  const spoof = (key: string, fake: unknown) => {
    const real = protoGetter(key);
    if (!real) return;
    try {
      Object.defineProperty(document, key, {
        configurable: true,
        enumerable: true,
        get: () => (active ? fake : real.call(document)),
      });
    } catch {
      /* non-configurable on some Chromium builds */
    }
  };
  spoof("hidden", false);
  spoof("visibilityState", "visible");
  spoof("webkitHidden", false);
  spoof("webkitVisibilityState", "visible");
  try {
    const realHasFocus = document.hasFocus.bind(document);
    (document as Document & { hasFocus: () => boolean }).hasFocus = () =>
      active ? true : realHasFocus();
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
    if (!active) return;
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

  // Endpoints that cancel an in-flight chat. Prefix-agnostic: the octane
  // front-end calls `/stop_conversation` without `/backend-api`.
  const STOP_RE = /\/(?:backend-api\/|backend-anon\/)?(?:stop_conversation|conversation\/stop|f\/conversation\/finalize)(?:[?#]|$)/;
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
  // Only the answer stream itself — not `/f/conversation/prepare`, `/init`
  // or the `/conversations` list, which share the prefix.
  const CONVERSATION_RE = /\/backend-api\/(f\/)?conversation\/?(\?|$)/;
  const postToContent = (streamEpoch: number, payload: object) => {
    try {
      window.postMessage(
        { __cgptBridge: true, epoch: streamEpoch, ...payload },
        window.location.origin,
      );
    } catch {
      /* */
    }
  };

  const teeAndParseSse = (response: Response, streamEpoch: number): Response => {
    if (!response.body) return response;
    const ct = response.headers.get("content-type") ?? "";
    if (ct.indexOf("event-stream") < 0) return response;
    const [pageStream, ourStream] = response.body.tee();
    const reader = ourStream.getReader();
    const decoder = new TextDecoder();
    const framer = new SseFramer();
    const state = newSseState();
    // This stream goes to the capture of its epoch, if any; a second
    // stream of the epoch starts with an SSE comment line.
    const keep = capture !== null && capture.epoch === streamEpoch ? capture : null;
    if (keep !== null && keep.streams++ > 0) captureNextStream(keep.kept, keep.streams);
    const feed = (ev: string): void => {
      if (keep === null) sseAccumulate(state, ev);
      else captureEvent(state, ev, keep.kept);
    };
    let lastReported = "";
    let chunkCount = 0;
    let doneSent = false;
    // Lets the content script hold its DOM-based "done" until the stream
    // has actually produced text (the DOM can look settled before that).
    postToContent(streamEpoch, { kind: "sse-start", text: "" });
    const reportIfChanged = () => {
      const text = answerText(state);
      if (text !== lastReported) {
        lastReported = text;
        postToContent(streamEpoch, { kind: "sse-tokens", text });
      }
    };
    // ChatGPT aborts the fetch right after `[DONE]`, so "done" is driven by
    // the stream's own completion markers, not by the body ending.
    const sendDone = () => {
      if (doneSent) return;
      doneSent = true;
      const text = answerText(state);
      debug("SSE done; answer chars=", text.length, "messages=", state.messages.length, "chunks=", chunkCount);
      postToContent(streamEpoch, { kind: "sse-done", text });
      reader.cancel().catch(() => {});
    };
    const pump = (): Promise<void> =>
      reader.read().then(({ done, value }) => {
        if (done) {
          for (const ev of framer.flush()) feed(ev);
          reportIfChanged();
          sendDone();
          return;
        }
        chunkCount++;
        for (const ev of framer.push(decoder.decode(value, { stream: true }))) {
          feed(ev);
        }
        reportIfChanged();
        if (isComplete(state)) {
          sendDone();
          return;
        }
        return pump();
      }).catch((err) => {
        if (doneSent) return;
        doneSent = true;
        // A read that breaks off (network cut, the page aborting its fetch)
        // carries no status: the content script then waits for the page's
        // own answer instead of failing (waitForNewAnswer, `streamCut`).
        debug("SSE pump error:", err);
        postToContent(streamEpoch, { kind: "sse-error", message: String(err) });
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
    if (active) {
      try {
        const url = urlOf(input);
        if (STOP_RE.test(url)) {
          debug("DROPPED stop_conversation request (URL):", url);
          return fakeOkResponse("stop_conversation suppressed to keep stream alive");
        }
        if (CONVERSATION_RE.test(url)) {
          debug("intercepting SSE conversation:", url);
          // Taken before the await: the shim may be switched over meanwhile.
          const streamEpoch = epoch;
          const response = await originalFetch(input, init);
          if (response.status === 429) {
            // ChatGPT throttles the account. The page shows its own notice;
            // the content script turns this into a `rate_limited` error.
            debug("conversation stream answered HTTP 429");
            postToContent(streamEpoch, { kind: "sse-error", status: 429, message: "HTTP 429 on the answer stream" });
            return response;
          }
          return teeAndParseSse(response, streamEpoch);
        }
      } catch (err) {
        debug("fetch shim error (passing through):", err);
      }
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
        if (active && STOP_RE.test(u)) {
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
      if (active && STOP_RE.test(url)) {
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

  debug("page shim installed (dormant)");
})();
