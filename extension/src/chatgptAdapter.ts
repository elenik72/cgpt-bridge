// =============================================================================
// ChatGPT DOM adapter
// -----------------------------------------------------------------------------
// All ChatGPT-specific DOM knowledge lives in this file. The rest of the
// codebase must not depend on ChatGPT selectors, class names, or DOM shape.
// When the page breaks, this is the only file that should need editing.
//
// Selector inventory (verified against https://chatgpt.com as of 2026-05):
//
//   Composer:
//     `div#prompt-textarea[contenteditable="true"]`
//        — ProseMirror editor. id is stable. `role="textbox"`, `aria-multiline`.
//     The sibling `<textarea class="wcDTda_fallbackTextarea" style="display:none">`
//     is a hidden fallback; isVisible() rejects it.
//
//   Send button (idle):
//     `button[data-testid="send-button"]`
//     also: `id="composer-submit-button"`, `aria-label="Send prompt"`,
//     class includes `composer-submit-btn composer-submit-button-color`.
//
//   Stop button (while streaming):
//     `button[data-testid="stop-button"]`
//     Same `id="composer-submit-button"` as send button. Distinguished by
//     `data-testid` flipping and class flipping to
//     `composer-secondary-button-color`. `aria-label="Stop answering"`.
//
//   Streaming-state marker (primary):
//     `[data-scroll-root][data-stream-active]` — scroll-root carries the
//     `data-stream-active` attribute exactly while streaming and removes it
//     when the turn completes. More reliable than watching the stop button
//     which can flicker on very short replies.
//
//   Assistant turn container (outer):
//     `section[data-turn="assistant"][data-turn-id="<uuid>"]`
//     with `data-testid="conversation-turn-N"`.
//
//   Assistant message (inner, payload carrier):
//     `[data-message-author-role="assistant"][data-message-id="<uuid>"]`
//     We use `data-message-id` as the baseline for "did a new turn appear".
//     Text-based baselines are fragile (regenerate, edit-in-place).
//
//   Rendered markdown body:
//     `.markdown` inside the assistant message. Targeting this drops the
//     sr-only `<h4>ChatGPT said:</h4>` chrome.
//
// Repair playbook — if any function stops working:
//
//   1. Run `diagnose()` from the SW console via the `"diagnose"` bridge
//      request (see content.ts). It reports which probe failed.
//   2. Open chatgpt.com DevTools, inspect the relevant element.
//   3. Update the offending selector constant. Prefer semantic selectors
//      (`role`, `aria-label`, `data-testid`, `id`) over Tailwind classes
//      or hashed class names (e.g. `wcDTda_*`).
//   4. Rebuild (`npm run build`), reload extension, reload chatgpt.com tab.
//
// Hard rules:
//   - Never use the system clipboard.
//   - Never depend on hashed/Tailwind class names.
//   - Never let ChatGPT selectors leak outside this file.
//   - Stabilization must include both a new `data-message-id` AND
//     `!isGenerating()` AND text-quiescence; any one of these alone is wrong.
// =============================================================================

import {
  AnswerTimeoutError,
  ComposerNotFoundError,
  SendButtonNotFoundError,
} from "./errors";

export function sleep(ms: number): Promise<void> {
  return new Promise((resolve) => setTimeout(resolve, ms));
}

// ---------------------------------------------------------------------------
// Hidden-tab keep-alive.
//
// Chrome aggressively throttles JavaScript in background tabs: timers run at
// most ~once per minute, requestAnimationFrame stops, and even some DOM
// mutations can be batched until the tab regains focus. Once a tab is
// considered "audible", however, those throttles are lifted.
//
// We exploit this by running a silent OscillatorNode at gain 0 inside the
// page's AudioContext for the duration of a `waitForNewAnswer` call. The
// browser marks the tab as audible and the ChatGPT streaming code keeps
// updating the DOM as if the tab were focused.
//
// Caveat: AudioContext.resume() requires a prior user gesture in the tab
// (Chrome autoplay policy). Loading chatgpt.com and never clicking on it
// will leave the context suspended; the helper logs once and falls through
// — throttled polling will still eventually finish, it will just be slow.
// ---------------------------------------------------------------------------

let keepAliveCtx: AudioContext | null = null;
let keepAliveOsc: OscillatorNode | null = null;
let keepAliveRefCount = 0;
let keepAliveWarned = false;

export function startKeepAlive(): void {
  keepAliveRefCount += 1;
  try {
    if (keepAliveCtx === null) {
      const Ctor =
        (window as unknown as { AudioContext?: typeof AudioContext })
          .AudioContext ??
        (window as unknown as { webkitAudioContext?: typeof AudioContext })
          .webkitAudioContext;
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
            "[cgpt-bridge] audio keep-alive could not start without a user gesture. " +
              "Click anywhere inside the ChatGPT tab once, then this tab will keep " +
              "streaming in the background.",
          );
        }
      });
    }
  } catch (err) {
    console.warn("[cgpt-bridge] keep-alive setup failed:", err);
  }
}

export function stopKeepAlive(): void {
  if (keepAliveRefCount > 0) keepAliveRefCount -= 1;
  if (keepAliveRefCount === 0 && keepAliveCtx !== null) {
    keepAliveCtx.suspend().catch(() => {});
  }
}

export async function waitUntil<T>(
  predicate: () => T | null | undefined | false,
  timeoutMs: number,
  intervalMs = 100,
): Promise<T> {
  const start = Date.now();
  while (Date.now() - start < timeoutMs) {
    const value = predicate();
    if (value) {
      return value as T;
    }
    await sleep(intervalMs);
  }
  throw new AnswerTimeoutError(`waitUntil timed out after ${timeoutMs}ms`);
}

export function normalizeText(text: string): string {
  return text.replace(/\s+/g, " ").trim();
}

export function isSupportedPage(): boolean {
  const host = location.host;
  return host === "chatgpt.com" || host.endsWith(".chatgpt.com");
}

// Selectors are intentionally semantic-first. ChatGPT class names are not
// stable and must never be hard-coded here.
const COMPOSER_SELECTORS: readonly string[] = [
  "div#prompt-textarea[contenteditable=\"true\"]",
  "div[contenteditable=\"true\"][data-testid*=\"composer\"]",
  "div[contenteditable=\"true\"][role=\"textbox\"]",
  "main form [contenteditable=\"true\"]",
  "textarea[data-testid*=\"prompt\"]",
  "main form textarea",
];

export function findComposer(): HTMLElement | null {
  for (const selector of COMPOSER_SELECTORS) {
    const candidates = document.querySelectorAll<HTMLElement>(selector);
    for (const el of Array.from(candidates)) {
      if (isVisible(el)) {
        return el;
      }
    }
  }
  return null;
}

function isVisible(el: HTMLElement): boolean {
  // offsetParent is null for elements with display:none or detached parents.
  // For position:fixed elements offsetParent may also be null, so fall back to
  // a bounding-rect check.
  if (el.offsetParent !== null) return true;
  const rect = el.getBoundingClientRect();
  return rect.width > 0 && rect.height > 0;
}

const SEND_BUTTON_SELECTORS: readonly string[] = [
  "button[data-testid=\"send-button\"]",
  "button[data-testid=\"fruitjuice-send-button\"]",
  "button[data-testid*=\"send\"]",
  "button[aria-label*=\"Send\" i]",
  "button[aria-label*=\"Отправ\" i]",
  "main form button[type=\"submit\"]",
];

function findSendButton(): HTMLButtonElement | null {
  for (const selector of SEND_BUTTON_SELECTORS) {
    const btn = document.querySelector<HTMLButtonElement>(selector);
    if (btn && !btn.disabled && isVisible(btn)) {
      return btn;
    }
  }
  return null;
}

export async function setComposerText(text: string): Promise<void> {
  const el = findComposer();
  if (!el) throw new ComposerNotFoundError();

  el.focus();

  if (el instanceof HTMLTextAreaElement) {
    setNativeTextareaValue(el, text);
    el.dispatchEvent(new Event("input", { bubbles: true }));
    el.dispatchEvent(new Event("change", { bubbles: true }));
    return;
  }

  // ProseMirror (ChatGPT's composer) does NOT trust direct DOM mutations
  // for its internal model — it listens to `beforeinput` events. The only
  // reliable way to drive it from JS is via `document.execCommand` which
  // synthesizes those events. We split on newlines and call insertParagraph
  // between lines so multi-line text round-trips as real paragraph breaks,
  // not literal "\n" chars inside one paragraph (which ProseMirror collapses
  // to a single line and submits only the first line).
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
    // ignore — empty composer
  }

  const lines = text.split("\n");
  for (let i = 0; i < lines.length; i++) {
    if (i > 0) {
      try {
        document.execCommand("insertParagraph");
      } catch {
        // Fallback: explicit DOM <p> if insertParagraph not supported.
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
        // Last-resort fallback: text node append.
        el.appendChild(document.createTextNode(line));
      }
    }
  }

  // Always fire one final `input` event so non-ProseMirror handlers (e.g.
  // the "Send" button's disabled-state watcher) re-evaluate.
  el.dispatchEvent(
    new InputEvent("input", {
      bubbles: true,
      cancelable: false,
      inputType: "insertText",
      data: text,
    }),
  );
}

function setNativeTextareaValue(el: HTMLTextAreaElement, value: string): void {
  // React/lit/etc. track a hidden "value tracker" on inputs. Using the native
  // setter is the only reliable way to push a new value past it.
  const proto = Object.getPrototypeOf(el) as object;
  const descriptor = Object.getOwnPropertyDescriptor(proto, "value");
  if (descriptor && typeof descriptor.set === "function") {
    descriptor.set.call(el, value);
  } else {
    el.value = value;
  }
}

export async function submit(): Promise<void> {
  // Give the framework a tick to enable the send button after the input event.
  const button = await waitForSendButton(5000);
  if (button) {
    button.click();
    return;
  }

  const composer = findComposer();
  if (!composer) throw new SendButtonNotFoundError();

  // Last-resort fallback: synthesize Enter on the composer. This is not the
  // primary path because the page may treat Enter differently in some modes.
  const keydown = new KeyboardEvent("keydown", {
    key: "Enter",
    code: "Enter",
    bubbles: true,
    cancelable: true,
  });
  const accepted = composer.dispatchEvent(keydown);
  if (!accepted) return;

  // If keydown was not cancelled, no submission likely occurred.
  throw new SendButtonNotFoundError(
    "Send button not found and Enter-key fallback was not consumed by the page.",
  );
}

async function waitForSendButton(timeoutMs: number): Promise<HTMLButtonElement | null> {
  const start = Date.now();
  while (Date.now() - start < timeoutMs) {
    const btn = findSendButton();
    if (btn) return btn;
    await sleep(100);
  }
  return null;
}

const ASSISTANT_MESSAGE_SELECTOR = "[data-message-author-role=\"assistant\"]";
const MARKDOWN_SELECTOR = ".markdown";

function lastAssistantNode(): HTMLElement | null {
  const nodes = document.querySelectorAll<HTMLElement>(ASSISTANT_MESSAGE_SELECTOR);
  if (nodes.length === 0) return null;
  return nodes[nodes.length - 1] ?? null;
}

export function getLastAssistantMessageId(): string | null {
  const last = lastAssistantNode();
  if (!last) return null;
  return last.dataset["messageId"] ?? null;
}

function readAssistantText(node: HTMLElement): string | null {
  // Prefer the rendered markdown container so we drop the sr-only "ChatGPT
  // said:" label and any non-content chrome attached to the message wrapper.
  const md = node.querySelector<HTMLElement>(MARKDOWN_SELECTOR);
  const source = md ?? node;

  // ChatGPT renders ```info … ``` fences as styled <pre><code class="language-info">
  // panels without the literal backticks. The Rust agent parser keys on
  // exactly those backticks + info string, so we walk the DOM and rebuild
  // the source markdown for code blocks. Other elements collapse to their
  // visible text via innerText.
  const parts: string[] = [];
  walkNodeForText(source, parts);
  const out = parts.join("").trim();
  return out.length > 0 ? out : null;
}

function walkNodeForText(node: Node, out: string[]): void {
  if (node.nodeType === Node.TEXT_NODE) {
    out.push(node.textContent ?? "");
    return;
  }
  if (node.nodeType !== Node.ELEMENT_NODE) return;
  const el = node as HTMLElement;
  const tag = el.tagName;

  // ChatGPT renders fenced code blocks in two different shapes depending on
  // model/feature flags:
  //   (a) CodeMirror viewer: <div id="code-block-viewer" class="cm-editor">
  //       with a sibling header containing the language label and a
  //       <pre class="cm-content"><code> with <span>+<br> per line.
  //   (b) Classic markdown: <pre><code class="language-X">…</code></pre>.
  // We synthesize triple-backtick fences for both so the Rust agent parser
  // can find them.
  if (el.id === "code-block-viewer" || el.classList.contains("cm-editor")) {
    const lang = findCodeBlockLanguage(el);
    const code =
      el.querySelector<HTMLElement>("pre.cm-content code") ??
      el.querySelector<HTMLElement>("code");
    if (code) {
      emitFence(lang, extractCodeText(code), out);
      return;
    }
  }

  if (tag === "PRE") {
    const code = el.querySelector<HTMLElement>("code");
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

  const isBlock =
    tag === "P" ||
    tag === "DIV" ||
    tag === "LI" ||
    tag === "H1" ||
    tag === "H2" ||
    tag === "H3" ||
    tag === "H4" ||
    tag === "H5" ||
    tag === "H6" ||
    tag === "UL" ||
    tag === "OL" ||
    tag === "BLOCKQUOTE";

  for (const child of Array.from(el.childNodes)) {
    walkNodeForText(child, out);
  }
  if (isBlock) out.push("\n");
}

function emitFence(lang: string, body: string, out: string[]): void {
  out.push("\n```" + lang + "\n");
  out.push(body);
  if (!body.endsWith("\n")) out.push("\n");
  out.push("```\n");
}

// CodeMirror puts each token in a <span> and each line break in a <br>.
// textContent loses the <br> newlines, so walk children explicitly.
function extractCodeText(code: HTMLElement): string {
  const parts: string[] = [];
  collectCodeText(code, parts);
  return parts.join("");
}

function collectCodeText(node: Node, out: string[]): void {
  if (node.nodeType === Node.TEXT_NODE) {
    out.push(node.textContent ?? "");
    return;
  }
  if (node.nodeType !== Node.ELEMENT_NODE) return;
  const el = node as HTMLElement;
  if (el.tagName === "BR") {
    out.push("\n");
    return;
  }
  for (const child of Array.from(el.childNodes)) {
    collectCodeText(child, out);
  }
}

// Locate the language label for a code block. ChatGPT puts it in a header
// element above the viewer; the safest heuristic is to walk a few levels
// up and look for any text matching our known `cgpt-*-v<N>` shape.
function findCodeBlockLanguage(viewer: HTMLElement): string {
  let cur: HTMLElement | null = viewer.parentElement;
  for (let depth = 0; cur && depth < 5; depth++) {
    const text = cur.textContent ?? "";
    const m = text.match(/cgpt-(?:agent-response|command-result)-v\d+/);
    if (m) return m[0];
    cur = cur.parentElement;
  }
  return "";
}

export function getLastAssistantMessage(): string | null {
  const last = lastAssistantNode();
  if (!last) return null;
  return readAssistantText(last);
}

export function isGenerating(): boolean {
  // Primary signal: ChatGPT sets `data-stream-active` on the scroll-root
  // exactly while a response is streaming, and clears it when streaming ends.
  if (document.querySelector("[data-scroll-root][data-stream-active]")) {
    return true;
  }
  // Fallback: the send button transforms into a stop button while streaming.
  const stop = document.querySelector(
    [
      "button[data-testid=\"stop-button\"]",
      "button[aria-label*=\"Stop\" i]",
      "button[aria-label*=\"Останов\" i]",
    ].join(","),
  );
  return stop !== null;
}

export interface WaitForAnswerOptions {
  stabilityMs?: number;
  pollIntervalMs?: number;
}

export async function waitForNewAnswer(
  baselineMessageId: string | null,
  timeoutMs = 120_000,
  options: WaitForAnswerOptions = {},
): Promise<string> {
  const stabilityMs = options.stabilityMs ?? 800;
  const pollIntervalMs = options.pollIntervalMs ?? 100;

  // Why MutationObserver instead of a plain `setTimeout` poll loop:
  // Chrome throttles timers in background tabs (minimum interval ~1 s).
  // When the user keeps their terminal in focus while ChatGPT streams in a
  // background tab, a 100 ms poll would effectively run at 1 Hz and the
  // user would see large latency. MutationObserver callbacks are NOT
  // throttled in background tabs, so streaming text is detected promptly.
  // We still keep a small setTimeout as a backstop so the final stability
  // check (no mutations for stabilityMs) can fire when the DOM has gone
  // quiet; in background tabs that backstop fires at ~1 s, which is fine
  // because stabilityMs is already in that ballpark.
  const start = Date.now();
  startKeepAlive();
  return new Promise<string>((resolve, reject) => {
    let lastSeenText: string | null = null;
    let stableSince = 0;
    let done = false;
    let observer: MutationObserver | null = null;
    let scheduled: ReturnType<typeof setTimeout> | null = null;
    let shimText: string | null = null;
    let shimDone = false;

    const finish = (err: Error | null, value: string | null): void => {
      if (done) return;
      done = true;
      if (observer !== null) observer.disconnect();
      if (scheduled !== null) clearTimeout(scheduled);
      window.removeEventListener("message", onShimMessage);
      stopKeepAlive();
      if (err !== null) reject(err);
      else resolve(value as string);
    };

    // Listen for the shim's SSE-hijack messages. Independent of DOM
    // rendering: the shim parses the conversation stream and forwards
    // every accumulated-text update plus a final `sse-done` event. When
    // the hidden tab's React scheduler is throttled and DOM mutations
    // stop arriving, this path still resolves the wait.
    const onShimMessage = (ev: MessageEvent): void => {
      if (ev.source !== window) return;
      const d = ev.data;
      if (!d || typeof d !== "object") return;
      if ((d as Record<string, unknown>).__cgptBridge !== true) return;
      const kind = (d as Record<string, unknown>).kind;
      const text = (d as Record<string, unknown>).text;
      if (typeof text === "string") {
        shimText = text;
        if (kind === "sse-done") {
          shimDone = true;
          if (text.length > 0) {
            finish(null, text);
          } else if (lastSeenText) {
            finish(null, lastSeenText);
          }
        }
      }
    };
    window.addEventListener("message", onShimMessage);

    const tick = (): void => {
      if (done) return;
      if (Date.now() - start > timeoutMs) {
        // Last-ditch: if the shim observed a complete SSE stream but the
        // DOM never rendered it (typical on hidden tabs with throttled
        // React), prefer the shim's text over a timeout error.
        if (shimDone && shimText) {
          finish(null, shimText);
          return;
        }
        if (shimText && shimText.length > 100) {
          finish(null, shimText);
          return;
        }
        finish(
          new AnswerTimeoutError(
            `Assistant response did not stabilize within ${timeoutMs}ms.`,
          ),
          null,
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
      // Reschedule. Mutations will also wake us via the observer; this is
      // the floor for the "no more mutations" stability check.
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
      characterData: true,
    });
    tick();
  });
}

// ---------------------------------------------------------------------------
// diagnose() — snapshot of what the adapter can see right now. Used by the
// `diagnose` bridge request from the SW console and (later) by `cgpt doctor`.
// Each probe returns `null`/`false` when missing so callers can pinpoint
// which selector to repair.
// ---------------------------------------------------------------------------

export interface AdapterDiagnostics {
  href: string;
  host: string;
  supportedPage: boolean;
  composerFound: boolean;
  composerTag: string | null;
  composerIsContentEditable: boolean | null;
  sendButtonFound: boolean;
  sendButtonDisabled: boolean | null;
  stopButtonFound: boolean;
  streamActiveAttrFound: boolean;
  isGenerating: boolean;
  assistantTurnCount: number;
  lastAssistantMessageId: string | null;
  lastAssistantTextLen: number;
  markdownContainerFound: boolean;
  userAgent: string;
  capturedAt: number;
}

export function diagnose(): AdapterDiagnostics {
  const composer = findComposer();
  const sendBtn = document.querySelector<HTMLButtonElement>(
    "button[data-testid=\"send-button\"]",
  );
  const stopBtn = document.querySelector<HTMLButtonElement>(
    "button[data-testid=\"stop-button\"]",
  );
  const streamActive = document.querySelector(
    "[data-scroll-root][data-stream-active]",
  );
  const assistantNodes = document.querySelectorAll(ASSISTANT_MESSAGE_SELECTOR);
  const lastNode = lastAssistantNode();
  const lastText = lastNode ? readAssistantText(lastNode) : null;
  const markdown = lastNode
    ? lastNode.querySelector<HTMLElement>(MARKDOWN_SELECTOR)
    : null;

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
    capturedAt: Date.now(),
  };
}
