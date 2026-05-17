(function() {
  "use strict";
  class CgptBridgeError extends Error {
    code;
    constructor(code, message) {
      super(message);
      this.name = "CgptBridgeError";
      this.code = code;
    }
  }
  class UnsupportedPageError extends CgptBridgeError {
    constructor(message = "Active tab is not a supported ChatGPT page.") {
      super("unsupported_page", message);
      this.name = "UnsupportedPageError";
    }
  }
  class ComposerNotFoundError extends CgptBridgeError {
    constructor(message = "ChatGPT composer input was not found in the DOM.") {
      super("composer_not_found", message);
      this.name = "ComposerNotFoundError";
    }
  }
  class SendButtonNotFoundError extends CgptBridgeError {
    constructor(message = "ChatGPT send button was not found or is disabled.") {
      super("send_button_not_found", message);
      this.name = "SendButtonNotFoundError";
    }
  }
  class AnswerTimeoutError extends CgptBridgeError {
    constructor(message = "Timed out waiting for a stable assistant response.") {
      super("answer_timeout", message);
      this.name = "AnswerTimeoutError";
    }
  }
  class UnknownExtensionError extends CgptBridgeError {
    constructor(message = "Unknown extension-internal error.") {
      super("unknown", message);
      this.name = "UnknownExtensionError";
    }
  }
  function toSerializableError(err) {
    if (err instanceof CgptBridgeError) {
      return { errorCode: err.code, message: err.message };
    }
    if (err instanceof Error) {
      return { errorCode: "unknown", message: err.message };
    }
    return { errorCode: "unknown", message: String(err) };
  }
  function sleep(ms) {
    return new Promise((resolve) => setTimeout(resolve, ms));
  }
  let keepAliveCtx = null;
  let keepAliveOsc = null;
  let keepAliveRefCount = 0;
  let keepAliveWarned = false;
  function startKeepAlive() {
    keepAliveRefCount += 1;
    try {
      if (keepAliveCtx === null) {
        const Ctor = window.AudioContext ?? window.webkitAudioContext;
        if (typeof Ctor !== "function") return;
        keepAliveCtx = new Ctor();
        const gain = keepAliveCtx.createGain();
        gain.gain.value = 0;
        keepAliveOsc = keepAliveCtx.createOscillator();
        keepAliveOsc.connect(gain);
        gain.connect(keepAliveCtx.destination);
        keepAliveOsc.start();
      }
      if (keepAliveCtx.state === "suspended") {
        keepAliveCtx.resume().catch(() => {
          if (!keepAliveWarned) {
            keepAliveWarned = true;
            console.warn(
              "[cgpt-bridge] audio keep-alive could not start without a user gesture. Click anywhere inside the ChatGPT tab once, then this tab will keep streaming in the background."
            );
          }
        });
      }
    } catch (err) {
      console.warn("[cgpt-bridge] keep-alive setup failed:", err);
    }
  }
  function stopKeepAlive() {
    if (keepAliveRefCount > 0) keepAliveRefCount -= 1;
    if (keepAliveRefCount === 0 && keepAliveCtx !== null) {
      keepAliveCtx.suspend().catch(() => {
      });
    }
  }
  function isSupportedPage() {
    const host = location.host;
    return host === "chatgpt.com" || host.endsWith(".chatgpt.com");
  }
  const COMPOSER_SELECTORS = [
    'div#prompt-textarea[contenteditable="true"]',
    'div[contenteditable="true"][data-testid*="composer"]',
    'div[contenteditable="true"][role="textbox"]',
    'main form [contenteditable="true"]',
    'textarea[data-testid*="prompt"]',
    "main form textarea"
  ];
  function findComposer() {
    for (const selector of COMPOSER_SELECTORS) {
      const candidates = document.querySelectorAll(selector);
      for (const el of Array.from(candidates)) {
        if (isVisible(el)) {
          return el;
        }
      }
    }
    return null;
  }
  function isVisible(el) {
    if (el.offsetParent !== null) return true;
    const rect = el.getBoundingClientRect();
    return rect.width > 0 && rect.height > 0;
  }
  const SEND_BUTTON_SELECTORS = [
    'button[data-testid="send-button"]',
    'button[data-testid="fruitjuice-send-button"]',
    'button[data-testid*="send"]',
    'button[aria-label*="Send" i]',
    'button[aria-label*="Отправ" i]',
    'main form button[type="submit"]'
  ];
  function findSendButton() {
    for (const selector of SEND_BUTTON_SELECTORS) {
      const btn = document.querySelector(selector);
      if (btn && !btn.disabled && isVisible(btn)) {
        return btn;
      }
    }
    return null;
  }
  async function setComposerText(text) {
    const el = findComposer();
    if (!el) throw new ComposerNotFoundError();
    el.focus();
    if (el instanceof HTMLTextAreaElement) {
      setNativeTextareaValue(el, text);
      el.dispatchEvent(new Event("input", { bubbles: true }));
      el.dispatchEvent(new Event("change", { bubbles: true }));
      return;
    }
    const selection = window.getSelection();
    if (selection) {
      const range = document.createRange();
      range.selectNodeContents(el);
      selection.removeAllRanges();
      selection.addRange(range);
    }
    try {
      document.execCommand("delete");
    } catch {
    }
    const lines = text.split("\n");
    for (let i = 0; i < lines.length; i++) {
      if (i > 0) {
        try {
          document.execCommand("insertParagraph");
        } catch {
          const p = document.createElement("p");
          p.appendChild(document.createElement("br"));
          el.appendChild(p);
        }
      }
      const line = lines[i] ?? "";
      if (line.length > 0) {
        try {
          document.execCommand("insertText", false, line);
        } catch {
          el.appendChild(document.createTextNode(line));
        }
      }
    }
    el.dispatchEvent(
      new InputEvent("input", {
        bubbles: true,
        cancelable: false,
        inputType: "insertText",
        data: text
      })
    );
  }
  function setNativeTextareaValue(el, value) {
    const proto = Object.getPrototypeOf(el);
    const descriptor = Object.getOwnPropertyDescriptor(proto, "value");
    if (descriptor && typeof descriptor.set === "function") {
      descriptor.set.call(el, value);
    } else {
      el.value = value;
    }
  }
  async function submit() {
    const button = await waitForSendButton(5e3);
    if (button) {
      button.click();
      return;
    }
    const composer = findComposer();
    if (!composer) throw new SendButtonNotFoundError();
    const keydown = new KeyboardEvent("keydown", {
      key: "Enter",
      code: "Enter",
      bubbles: true,
      cancelable: true
    });
    const accepted = composer.dispatchEvent(keydown);
    if (!accepted) return;
    throw new SendButtonNotFoundError(
      "Send button not found and Enter-key fallback was not consumed by the page."
    );
  }
  async function waitForSendButton(timeoutMs) {
    const start = Date.now();
    while (Date.now() - start < timeoutMs) {
      const btn = findSendButton();
      if (btn) return btn;
      await sleep(100);
    }
    return null;
  }
  const ASSISTANT_MESSAGE_SELECTOR = '[data-message-author-role="assistant"]';
  const MARKDOWN_SELECTOR = ".markdown";
  function lastAssistantNode() {
    const nodes = document.querySelectorAll(ASSISTANT_MESSAGE_SELECTOR);
    if (nodes.length === 0) return null;
    return nodes[nodes.length - 1] ?? null;
  }
  function getLastAssistantMessageId() {
    const last = lastAssistantNode();
    if (!last) return null;
    return last.dataset["messageId"] ?? null;
  }
  function readAssistantText(node) {
    const md = node.querySelector(MARKDOWN_SELECTOR);
    const source = md ?? node;
    const parts = [];
    walkNodeForText(source, parts);
    const out = parts.join("").trim();
    return out.length > 0 ? out : null;
  }
  function walkNodeForText(node, out) {
    if (node.nodeType === Node.TEXT_NODE) {
      out.push(node.textContent ?? "");
      return;
    }
    if (node.nodeType !== Node.ELEMENT_NODE) return;
    const el = node;
    const tag = el.tagName;
    if (el.id === "code-block-viewer" || el.classList.contains("cm-editor")) {
      const lang = findCodeBlockLanguage(el);
      const code = el.querySelector("pre.cm-content code") ?? el.querySelector("code");
      if (code) {
        emitFence(lang, extractCodeText(code), out);
        return;
      }
    }
    if (tag === "PRE") {
      const code = el.querySelector("code");
      if (code) {
        let lang = "";
        for (const cls of Array.from(code.classList)) {
          if (cls.startsWith("language-")) {
            lang = cls.substring("language-".length);
            break;
          }
        }
        if (lang === "") {
          lang = findCodeBlockLanguage(el);
        }
        emitFence(lang, extractCodeText(code), out);
        return;
      }
    }
    if (tag === "BR") {
      out.push("\n");
      return;
    }
    const isBlock = tag === "P" || tag === "DIV" || tag === "LI" || tag === "H1" || tag === "H2" || tag === "H3" || tag === "H4" || tag === "H5" || tag === "H6" || tag === "UL" || tag === "OL" || tag === "BLOCKQUOTE";
    for (const child of Array.from(el.childNodes)) {
      walkNodeForText(child, out);
    }
    if (isBlock) out.push("\n");
  }
  function emitFence(lang, body, out) {
    out.push("\n```" + lang + "\n");
    out.push(body);
    if (!body.endsWith("\n")) out.push("\n");
    out.push("```\n");
  }
  function extractCodeText(code) {
    const parts = [];
    collectCodeText(code, parts);
    return parts.join("");
  }
  function collectCodeText(node, out) {
    if (node.nodeType === Node.TEXT_NODE) {
      out.push(node.textContent ?? "");
      return;
    }
    if (node.nodeType !== Node.ELEMENT_NODE) return;
    const el = node;
    if (el.tagName === "BR") {
      out.push("\n");
      return;
    }
    for (const child of Array.from(el.childNodes)) {
      collectCodeText(child, out);
    }
  }
  function findCodeBlockLanguage(viewer) {
    let cur = viewer.parentElement;
    for (let depth = 0; cur && depth < 5; depth++) {
      const text = cur.textContent ?? "";
      const m = text.match(/cgpt-(?:agent-response|command-result)-v\d+/);
      if (m) return m[0];
      cur = cur.parentElement;
    }
    return "";
  }
  function isGenerating() {
    if (document.querySelector("[data-scroll-root][data-stream-active]")) {
      return true;
    }
    const stop = document.querySelector(
      [
        'button[data-testid="stop-button"]',
        'button[aria-label*="Stop" i]',
        'button[aria-label*="Останов" i]'
      ].join(",")
    );
    return stop !== null;
  }
  async function waitForNewAnswer(baselineMessageId, timeoutMs = 12e4, options = {}) {
    const stabilityMs = options.stabilityMs ?? 800;
    const pollIntervalMs = options.pollIntervalMs ?? 100;
    const start = Date.now();
    startKeepAlive();
    return new Promise((resolve, reject) => {
      let lastSeenText = null;
      let stableSince = 0;
      let done = false;
      let observer = null;
      let scheduled = null;
      const finish = (err, value) => {
        if (done) return;
        done = true;
        if (observer !== null) observer.disconnect();
        if (scheduled !== null) clearTimeout(scheduled);
        stopKeepAlive();
        if (err !== null) reject(err);
        else resolve(value);
      };
      const tick = () => {
        if (done) return;
        if (Date.now() - start > timeoutMs) {
          finish(
            new AnswerTimeoutError(
              `Assistant response did not stabilize within ${timeoutMs}ms.`
            ),
            null
          );
          return;
        }
        const node = lastAssistantNode();
        const currentId = node?.dataset["messageId"] ?? null;
        const isNewTurn = node !== null && currentId !== baselineMessageId;
        if (isNewTurn) {
          const currentText = readAssistantText(node);
          if (currentText && currentText === lastSeenText) {
            if (Date.now() - stableSince >= stabilityMs && !isGenerating()) {
              finish(null, currentText);
              return;
            }
          } else if (currentText) {
            lastSeenText = currentText;
            stableSince = Date.now();
          }
        }
        if (scheduled !== null) clearTimeout(scheduled);
        scheduled = setTimeout(tick, pollIntervalMs);
      };
      observer = new MutationObserver(() => {
        if (scheduled !== null) {
          clearTimeout(scheduled);
          scheduled = null;
        }
        tick();
      });
      observer.observe(document.body, {
        childList: true,
        subtree: true,
        characterData: true
      });
      tick();
    });
  }
  function diagnose() {
    const composer = findComposer();
    const sendBtn = document.querySelector(
      'button[data-testid="send-button"]'
    );
    const stopBtn = document.querySelector(
      'button[data-testid="stop-button"]'
    );
    const streamActive = document.querySelector(
      "[data-scroll-root][data-stream-active]"
    );
    const assistantNodes = document.querySelectorAll(ASSISTANT_MESSAGE_SELECTOR);
    const lastNode = lastAssistantNode();
    const lastText = lastNode ? readAssistantText(lastNode) : null;
    const markdown = lastNode ? lastNode.querySelector(MARKDOWN_SELECTOR) : null;
    return {
      href: location.href,
      host: location.host,
      supportedPage: isSupportedPage(),
      composerFound: composer !== null,
      composerTag: composer ? composer.tagName.toLowerCase() : null,
      composerIsContentEditable: composer ? composer.isContentEditable : null,
      sendButtonFound: sendBtn !== null,
      sendButtonDisabled: sendBtn ? sendBtn.disabled : null,
      stopButtonFound: stopBtn !== null,
      streamActiveAttrFound: streamActive !== null,
      isGenerating: isGenerating(),
      assistantTurnCount: assistantNodes.length,
      lastAssistantMessageId: lastNode?.dataset["messageId"] ?? null,
      lastAssistantTextLen: lastText?.length ?? 0,
      markdownContainerFound: markdown !== null,
      userAgent: navigator.userAgent,
      capturedAt: Date.now()
    };
  }
  chrome.runtime.onMessage.addListener(
    (message, _sender, sendResponse) => {
      const req = parseRequest(message);
      if (!req) {
        sendResponse({
          id: "unknown",
          ok: false,
          errorCode: "bad_request",
          message: "Content script received an unrecognized message shape."
        });
        return false;
      }
      handleRequest(req).then((response) => sendResponse(response)).catch((err) => {
        const ser = toSerializableError(err);
        sendResponse({
          id: req.id,
          ok: false,
          errorCode: ser.errorCode,
          message: ser.message
        });
      });
      return true;
    }
  );
  function parseRequest(value) {
    if (typeof value !== "object" || value === null) return null;
    const v = value;
    if (typeof v.id !== "string") return null;
    if (v.type === "test.ask" && typeof v.text === "string" && typeof v.timeoutMs === "number") {
      return {
        id: v.id,
        type: "test.ask",
        text: v.text,
        timeoutMs: v.timeoutMs
      };
    }
    if (v.type === "diagnose") {
      return { id: v.id, type: "diagnose" };
    }
    return null;
  }
  async function handleRequest(req) {
    switch (req.type) {
      case "test.ask":
        return handleTestAsk(req);
      case "diagnose":
        return handleDiagnose(req);
      default: {
        const _exhaustive = req;
        throw new UnknownExtensionError(
          `Unhandled request: ${JSON.stringify(_exhaustive)}`
        );
      }
    }
  }
  async function handleTestAsk(req) {
    if (!isSupportedPage()) {
      throw new UnsupportedPageError(
        `Content script is running on an unsupported host: ${location.host}`
      );
    }
    startKeepAlive();
    let text;
    try {
      const baselineMessageId = getLastAssistantMessageId();
      await setComposerText(req.text);
      await submit();
      text = await waitForNewAnswer(baselineMessageId, req.timeoutMs);
    } finally {
      stopKeepAlive();
    }
    console.log(
      "[cgpt-bridge][content] assistant text (first 400 chars):\n" + text.slice(0, 400) + (text.length > 400 ? "\n... [+" + (text.length - 400) + " chars]" : "")
    );
    return { id: req.id, ok: true, kind: "test.ask", text };
  }
  async function handleDiagnose(req) {
    const diagnostics = diagnose();
    return { id: req.id, ok: true, kind: "diagnose", diagnostics };
  }
})();
//# sourceMappingURL=content.js.map
