(function() {
  "use strict";
  (() => {
    const W = window;
    if (W.__cgptBridgePageShim) return;
    W.__cgptBridgePageShim = true;
    const TAG = "[cgpt-bridge:shim]";
    const debug = (...args) => {
      try {
        console.info(TAG, ...args);
      } catch {
      }
    };
    const def = (obj, key, getter) => {
      try {
        Object.defineProperty(obj, key, {
          configurable: true,
          enumerable: true,
          get: getter
        });
      } catch {
      }
    };
    def(document, "hidden", () => false);
    def(document, "visibilityState", () => "visible");
    def(document, "webkitHidden", () => false);
    def(document, "webkitVisibilityState", () => "visible");
    try {
      document.hasFocus = () => true;
    } catch {
    }
    const BLOCKED_EVENTS = [
      "visibilitychange",
      "webkitvisibilitychange",
      "blur",
      "pagehide",
      "freeze"
    ];
    const swallow = (e) => {
      try {
        e.stopImmediatePropagation();
      } catch {
      }
      try {
        e.preventDefault();
      } catch {
      }
    };
    for (const t of BLOCKED_EVENTS) {
      document.addEventListener(t, swallow, true);
      window.addEventListener(t, swallow, true);
    }
    const urlOf = (input) => {
      if (typeof input === "string") return input;
      if (input instanceof URL) return input.toString();
      if (input instanceof Request) return input.url;
      return String(input);
    };
    const STOP_RE = /\/backend-api\/(stop_conversation|conversation\/stop|f\/conversation\/finalize)/;
    const fakeOkResponse = (note) => {
      const body = JSON.stringify({
        success: true,
        blocked_by: "cgpt-bridge",
        note
      });
      return new Response(body, {
        status: 200,
        statusText: "OK",
        headers: { "content-type": "application/json" }
      });
    };
    const CONVERSATION_RE = /\/backend-api\/(f\/)?conversation(\?|$|\/?)/;
    const postToContent = (payload) => {
      try {
        window.postMessage(
          { __cgptBridge: true, ...payload },
          window.location.origin
        );
      } catch {
      }
    };
    const sseAccumulate = (state, eventData) => {
      if (!eventData || eventData === "[DONE]") return;
      let obj;
      try {
        obj = JSON.parse(eventData);
      } catch {
        return;
      }
      const visit = (o) => {
        if (o === null || typeof o !== "object") return;
        const node = o;
        const msg = node.message;
        if (msg && typeof msg === "object") {
          const content = msg.content;
          if (content) {
            const parts = content.parts;
            if (Array.isArray(parts) && parts.length > 0 && typeof parts[0] === "string") {
              state.text = parts[0];
            }
          }
        }
        if (typeof node.v === "string" && node.o === void 0 && !msg) {
          state.text += node.v;
        }
        if (typeof node.p === "string" && typeof node.v === "string") {
          if (node.p.indexOf("/message/content/parts/0") >= 0) {
            if (node.o === "append" || node.o === void 0) {
              state.text += node.v;
            } else if (node.o === "replace") {
              state.text = node.v;
            }
          }
        }
        if (node.o === "patch" && Array.isArray(node.v)) {
          for (const sub of node.v) visit(sub);
        }
      };
      visit(obj);
    };
    const teeAndParseSse = (response) => {
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
            text: state.text
          });
        }
      };
      const pump = () => reader.read().then(({ done, value }) => {
        if (done) {
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
        let nl;
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
        headers: response.headers
      });
    };
    const originalFetch = window.fetch.bind(window);
    window.fetch = async function patchedFetch(input, init) {
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
    };
    try {
      const origSendBeacon = navigator.sendBeacon.bind(navigator);
      navigator.sendBeacon = function patchedSendBeacon(url, data) {
        try {
          const u = typeof url === "string" ? url : url.toString();
          if (STOP_RE.test(u)) {
            debug("DROPPED stop_conversation sendBeacon:", u);
            return true;
          }
        } catch (err) {
          debug("sendBeacon shim error (passing through):", err);
        }
        return origSendBeacon(url, data);
      };
    } catch (err) {
      debug("sendBeacon wrap install failed:", err);
    }
    const xhrOpen = XMLHttpRequest.prototype.open;
    XMLHttpRequest.prototype.open = function patchedOpen(method, url, ...rest) {
      this.__cgptUrl = typeof url === "string" ? url : url.toString();
      return xhrOpen.call(this, method, url, ...rest);
    };
    const xhrSend = XMLHttpRequest.prototype.send;
    XMLHttpRequest.prototype.send = function patchedSend(body) {
      try {
        const url = this.__cgptUrl ?? "";
        if (STOP_RE.test(url)) {
          debug("DROPPED stop_conversation XHR:", url);
          try {
            Object.defineProperty(this, "readyState", { value: 4, configurable: true });
            Object.defineProperty(this, "status", { value: 200, configurable: true });
            Object.defineProperty(this, "statusText", { value: "OK", configurable: true });
            Object.defineProperty(this, "responseText", {
              value: '{"success":true,"blocked_by":"cgpt-bridge"}',
              configurable: true
            });
            Object.defineProperty(this, "response", {
              value: '{"success":true,"blocked_by":"cgpt-bridge"}',
              configurable: true
            });
            this.dispatchEvent(new Event("readystatechange"));
            this.dispatchEvent(new Event("load"));
            this.dispatchEvent(new Event("loadend"));
          } catch {
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
})();
//# sourceMappingURL=pageShim.js.map
