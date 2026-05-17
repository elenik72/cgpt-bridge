(function() {
  "use strict";
  function newRequestId() {
    const rand = Math.random().toString(36).slice(2, 10);
    return `req_${Date.now().toString(36)}_${rand}`;
  }
  const NATIVE_HOST_NAME = "com.cgpt_bridge.host";
  function pingNative(options = {}) {
    const timeoutMs = options.timeoutMs ?? 5e3;
    const requestId = `ping_${Date.now().toString(36)}_${Math.random().toString(36).slice(2, 8)}`;
    const startedAt = Date.now();
    return new Promise((resolve) => {
      let settled = false;
      let port2 = null;
      let timer = null;
      const settle = (outcome) => {
        if (settled) return;
        settled = true;
        if (timer !== null) clearTimeout(timer);
        try {
          port2?.disconnect();
        } catch {
        }
        resolve(outcome);
      };
      try {
        port2 = chrome.runtime.connectNative(NATIVE_HOST_NAME);
      } catch (err) {
        settle({
          ok: false,
          durationMs: Date.now() - startedAt,
          errorCode: "connect_native_threw",
          message: err instanceof Error ? err.message : String(err)
        });
        return;
      }
      port2.onMessage.addListener((msg) => {
        settle({
          ok: true,
          durationMs: Date.now() - startedAt,
          response: msg
        });
      });
      port2.onDisconnect.addListener(() => {
        const lastError = chrome.runtime.lastError;
        settle({
          ok: false,
          durationMs: Date.now() - startedAt,
          errorCode: "disconnected",
          message: lastError?.message ?? "Native host disconnected before sending a response. Verify the manifest is installed and the extension id in allowed_origins matches this extension."
        });
      });
      timer = setTimeout(() => {
        settle({
          ok: false,
          durationMs: Date.now() - startedAt,
          errorCode: "timeout",
          message: `Native host did not respond within ${timeoutMs}ms.`
        });
      }, timeoutMs);
      try {
        port2.postMessage({
          type: "ping",
          id: requestId,
          payload: options.payload ?? { from: "cgpt-bridge-extension" }
        });
      } catch (err) {
        settle({
          ok: false,
          durationMs: Date.now() - startedAt,
          errorCode: "post_message_threw",
          message: err instanceof Error ? err.message : String(err)
        });
      }
    });
  }
  const KEEPALIVE_INTERVAL_MS = 2e4;
  const RECONNECT_BACKOFF_MS = 5e3;
  const MAX_RECONNECT_DELAY_MS = 6e4;
  const CHATGPT_URL_PREFIX$1 = "https://chatgpt.com/";
  let port = null;
  let keepaliveTimer = null;
  let reconnectAttempts = 0;
  let started = false;
  function startBridge() {
    if (started) return;
    started = true;
    console.log("[cgpt-bridge] bridge starting");
    openPort();
  }
  function openPort() {
    if (port !== null) return;
    let next;
    try {
      next = chrome.runtime.connectNative(NATIVE_HOST_NAME);
    } catch (err) {
      console.error(
        "[cgpt-bridge] connectNative threw — is the native host installed?",
        err
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
        lastError?.message ?? "(no lastError)"
      );
      teardown();
      scheduleReconnect();
    });
    startKeepalive();
  }
  function teardown() {
    if (keepaliveTimer !== null) {
      clearInterval(keepaliveTimer);
      keepaliveTimer = null;
    }
    port = null;
  }
  function startKeepalive() {
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
  function scheduleReconnect() {
    reconnectAttempts += 1;
    const delay = Math.min(
      RECONNECT_BACKOFF_MS * reconnectAttempts,
      MAX_RECONNECT_DELAY_MS
    );
    console.log(
      `[cgpt-bridge] scheduling reconnect attempt #${reconnectAttempts} in ${delay} ms`
    );
    setTimeout(openPort, delay);
  }
  function handleHostMessage(raw) {
    const msg = raw;
    if (!msg || typeof msg !== "object" || typeof msg.type !== "string") {
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
        return;
      default: {
        const unknown = msg.type;
        console.warn(`[cgpt-bridge] ignoring host message of type ${String(unknown)}`);
      }
    }
  }
  async function handleAskFromHost(ask) {
    const respond = (payload) => {
      if (port === null) {
        console.warn(
          `[cgpt-bridge] cannot respond to ask ${ask.id}: native port closed`
        );
        return;
      }
      try {
        port.postMessage(payload);
      } catch (err) {
        console.error(
          `[cgpt-bridge] failed to post response for ask ${ask.id}:`,
          err
        );
      }
    };
    let tab;
    try {
      tab = await findActiveChatGptTab();
    } catch (err) {
      respond({
        type: "error",
        id: ask.id,
        code: "tab_unavailable",
        message: err instanceof Error ? err.message : String(err)
      });
      return;
    }
    if (typeof tab.id !== "number") {
      respond({
        type: "error",
        id: ask.id,
        code: "tab_unavailable",
        message: "active chatgpt.com tab has no id"
      });
      return;
    }
    const tabRequest = {
      id: newRequestId(),
      type: "test.ask",
      text: ask.text,
      timeoutMs: ask.timeout_ms
    };
    const tabResponse = await sendToTab$1(tab.id, tabRequest);
    if (tabResponse.ok) {
      if (tabResponse.kind === "test.ask") {
        respond({ type: "ask_result", id: ask.id, text: tabResponse.text });
      } else {
        respond({
          type: "error",
          id: ask.id,
          code: "internal",
          message: `unexpected tab response kind ${tabResponse.kind}`
        });
      }
    } else {
      respond({
        type: "error",
        id: ask.id,
        code: mapTabErrorCode(tabResponse.errorCode),
        message: tabResponse.message
      });
    }
  }
  function mapTabErrorCode(contentCode) {
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
  async function findActiveChatGptTab() {
    const queries = [
      { active: true, lastFocusedWindow: true, url: `${CHATGPT_URL_PREFIX$1}*` },
      { active: true, url: `${CHATGPT_URL_PREFIX$1}*` },
      { url: `${CHATGPT_URL_PREFIX$1}*` }
    ];
    for (const q of queries) {
      const tabs = await chrome.tabs.query(q);
      const tab = tabs.find(
        (t) => typeof t.id === "number" && typeof t.url === "string" && t.url.startsWith(CHATGPT_URL_PREFIX$1)
      );
      if (tab) {
        return tab;
      }
    }
    throw new Error(
      "no https://chatgpt.com/ tab is open — open chatgpt.com in any Chrome window"
    );
  }
  function sendToTab$1(tabId, req) {
    return new Promise((resolve) => {
      chrome.tabs.sendMessage(tabId, req, (resp) => {
        const lastError = chrome.runtime.lastError;
        if (lastError) {
          resolve({
            id: req.id,
            ok: false,
            errorCode: "content_script_not_ready",
            message: `${lastError.message ?? "lastError"} (try reloading the ChatGPT tab after installing or reloading the extension)`
          });
          return;
        }
        if (!resp) {
          resolve({
            id: req.id,
            ok: false,
            errorCode: "content_script_not_ready",
            message: "content script returned no response"
          });
          return;
        }
        resolve(resp);
      });
    });
  }
  startBridge();
  chrome.runtime.onStartup.addListener(() => startBridge());
  chrome.runtime.onInstalled.addListener(() => startBridge());
  const TEST_PROMPT = 'Say "pong" in one short sentence.';
  const DEFAULT_TIMEOUT_MS = 12e4;
  const CHATGPT_URL_PREFIX = "https://chatgpt.com/";
  const BADGE_CLEAR_MS = 4e3;
  chrome.action.onClicked.addListener((tab) => {
    void runTestAsk(tab);
  });
  async function runTestAsk(tab) {
    try {
      await setBadge("...", "#808080");
      if (typeof tab.id !== "number") {
        throw new Error("Active tab has no id.");
      }
      if (typeof tab.url !== "string" || !tab.url.startsWith(CHATGPT_URL_PREFIX)) {
        throw new Error(
          `Active tab is not on ${CHATGPT_URL_PREFIX}. Current URL: ${tab.url ?? "(unknown)"}`
        );
      }
      const request = {
        id: newRequestId(),
        type: "test.ask",
        text: TEST_PROMPT,
        timeoutMs: DEFAULT_TIMEOUT_MS
      };
      console.log("[cgpt-bridge] sending test prompt:", request);
      const response = await sendToTab(tab.id, request);
      if (response.ok) {
        if (response.kind === "test.ask") {
          console.log("[cgpt-bridge] assistant response:");
          console.log(response.text);
        } else {
          console.log("[cgpt-bridge] unexpected ok response kind:", response);
        }
        await setBadge("OK", "#0a7d28");
      } else {
        console.error(
          "[cgpt-bridge] content script reported error:",
          response.errorCode,
          response.message
        );
        await setBadge("ERR", "#a01a1a");
      }
    } catch (err) {
      console.error("[cgpt-bridge] background error:", err);
      await setBadge("ERR", "#a01a1a");
    } finally {
      setTimeout(() => {
        void clearBadge();
      }, BADGE_CLEAR_MS);
    }
  }
  function sendToTab(tabId, request) {
    return new Promise((resolve, reject) => {
      chrome.tabs.sendMessage(
        tabId,
        request,
        (response) => {
          const lastError = chrome.runtime.lastError;
          if (lastError) {
            reject(
              new Error(
                `chrome.runtime.lastError: ${lastError.message ?? "unknown"}. Try reloading the ChatGPT tab after installing/reloading the extension.`
              )
            );
            return;
          }
          if (!response) {
            reject(new Error("Empty response from content script."));
            return;
          }
          resolve(response);
        }
      );
    });
  }
  async function setBadge(text, color) {
    await chrome.action.setBadgeBackgroundColor({ color });
    await chrome.action.setBadgeText({ text });
  }
  async function clearBadge() {
    await chrome.action.setBadgeText({ text: "" });
  }
  self["pingNative"] = async (payload) => {
    const out = await pingNative(payload === void 0 ? {} : { payload });
    console.log("[cgpt-bridge] pingNative:", out);
    return out;
  };
})();
//# sourceMappingURL=background.js.map
