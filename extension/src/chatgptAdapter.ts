// =============================================================================
// ChatGPT DOM adapter
// -----------------------------------------------------------------------------
// All ChatGPT-specific DOM knowledge lives in this file. The rest of the
// codebase must not depend on ChatGPT selectors, class names, or DOM shape.
// When the page breaks, this is the only file that should need editing.
//
// ChatGPT currently serves three different markups. The adapter detects
// which one is on the page (`detectProfile()`) and every probe below accepts
// all three, so a rollout of a new markup degrades to a clear error instead
// of a silent timeout.
//
// Profile "account" — signed-in web app (verified 2026-10-23):
//   Composer:   `div.ProseMirror[contenteditable="true"][role="textbox"]`
//               with `data-composer-markdown`; no id.
//   Send/Stop:  send is `button[type="submit"]` inside the composer
//               `<form>`, no data-testid/id, absent while the composer is
//               empty. While streaming a SEPARATE `button[type="button"]`
//               labelled Stop takes its place (no distinguishing attribute
//               besides the localized label); gone when the turn ends.
//   Turn:       `[data-talvt-turn-state]` (user bubble + answer). Observed
//               sequence after submit: `complete` (briefly, before the reply
//               starts) → `in_progress` → `complete` | `cancelled`. So
//               `complete` alone does NOT mean "done"; the language-independent
//               guard is the pageShim stream (see waitForNewAnswer). The
//               user's message is `[data-user-message-bubble]` inside it.
//   Answer:     `[data-dil-source-message-id][data-markdown-copy]`, one per
//               turn; the attribute carries the message id. The sibling
//               `h4[data-conversation-role="assistant"]` is a screen-reader
//               label and must not leak into the text.
//   2026-10-28 (live check, research/2026-10-28-live-sse-and-markup.md):
//               the answer's id is on containers
//               (`[data-dil-source-message-id]`,
//               `[data-chatgpt-selection-message-id]`, also
//               `[data-dil-message-id]`), its text in an inner
//               `[data-markdown-copy]` with `p[data-d-text-flow]`
//               paragraphs. User messages: one `[data-user-message-bubble]`
//               each. The composer's label is localized. How the turn
//               states nest, the values a running turn shows, the Stop
//               button and code blocks were not recorded on this markup.
//               So an id container counts as an answer only with a
//               `[data-markdown-copy]` in it (the last one is read: the
//               final channel follows any reasoning recap), and only below
//               the newest user message.
//   Code block: `[data-code-block="true"]` with a header
//               `[data-markdown-copy="exclude"]` holding the language label
//               as a plain leaf (display-cased: "Bash"; protocol labels kept
//               as typed) and the code in `[data-code-block-source] > code`
//               (newlines as text, `<span>` tokens, no `language-*` class).
//               An earlier variant (also seen 2026-10-23) used `pre > code`.
//   Citations:  a badge (`[data-d-component="badge"]`) inside a
//               `[data-d-component="popover-trigger"]`; its label ("Rust
//               Blog") is not part of the answer.
//   Stream:     SSE JSON on `/backend-api/f/conversation` (pageShim). The
//               DOM settles a few seconds before the stream completes; the
//               stream text is the source markdown, so it is preferred.
//   Sidebar (verified 2026-10-26):
//               "New chat" is the first `button[data-sidebar-cycle-item]`
//               without `data-sidebar-destination` (other cycle items carry
//               `builtin:library|automations|skills` or open menus); its
//               aria-label is localized, no data-testid. A collapsed sidebar
//               shows a plain `button[aria-label=<New chat>]` instead.
//               Chat links: `a[href^="/c/"][data-interactive-row-link="true"]`,
//               only the ~20 most recent chats are rendered.
//   URL:        a new chat is `/`; after submit it moves to the temporary
//               `/c/local-chatgpt:<uuid>`, then to the saved `/c/<id>`.
//   Projects (verified 2026-10-28, research/2026-10-28-chatgpt-projects-live.md):
//               sidebar row `div[role=button][data-app-action-sidebar-project-row]`
//               with the name in `data-app-action-sidebar-project-label` and
//               the id (`g-p-<32 hex>`) in `data-app-action-sidebar-project-id`;
//               the project page is `/g/<id>/project` (no name in it), with
//               the composer. "Create" is
//               `button[data-app-action-sidebar-project-create]` (localized
//               label): a `[role=dialog]` with `input#chatgpt-project-name`
//               and `button[type=submit]`; the page then moves to the new
//               project's page. A chat started there is
//               `/g/<id>-<slug>/c/<chat id>` (slug: the name). No reliable
//               sign of a collapsed "Projects" section was recorded: the
//               `[aria-expanded=false][data-state=closed]` ancestor of the
//               create button is the header's menu, not the section (live
//               check 2026-10-28), so nothing is clicked to "open" it. A
//               long list shows the five newest projects and a "show more"
//               button (see showMoreProjectsButton), which openProject
//               presses once when the row is not in view.
//   Rate limit: no page text observed; detected as HTTP 429 on the answer
//               stream (pageShim → `sse-error` with `status: 429`).
//   Attachments (T6, 2026-11-06 — NOT verified live; see attachFiles):
//               files go in through an `input[type=file]` of the composer's
//               own form (DataTransfer + `change`), else a synthetic `paste`
//               on the composer (a DOM event: the system clipboard is never
//               touched); nothing outside that form is used (a project page
//               has its own file upload), and without the form nothing is
//               attached. A tile per file is counted by its remove button
//               (`aria-label` "Remove file…"/"Удалить файл…", or a
//               `data-testid` with remove-file) or a `blob:` preview image;
//               an upload counts as running while the form shows
//               `[role=progressbar]` or `[aria-busy=true]`. A live check has
//               to confirm the input, the tile and the progress signs, and
//               that Send stays disabled until the upload ends.
//
// Profile "octane" — new front-end served to signed-out users (2026-10-23):
//   Composer:   `textarea[name="prompt"]` / `[data-composer-editor-host]`.
//   Send/Stop:  `button[data-composer-submit]`; disabled via `aria-disabled`
//               and `data-visually-disabled` (not `disabled`); carries
//               `data-send-label` / `data-stop-label`.
//   Streaming:  `[data-conversation-stream-active]`, `[data-message-streaming]`.
//   Answer:     `[data-message-role="assistant"]`, body
//               `[data-assistant-markdown-root]`; reasoning lives in
//               `[data-assistant-reasoning-summary|transient]`.
//   Stream:     server-owned HTML patches, not SSE JSON — DOM only.
//
// Profile "classic" — the markup this adapter was first written against
// (2026-05): `div#prompt-textarea`, `button[data-testid="send-button"|
// "stop-button"]`, `[data-scroll-root][data-stream-active]`,
// `[data-message-author-role="assistant"][data-message-id]`, `.markdown`,
// CodeMirror `#code-block-viewer` or `pre > code.language-X`.
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

import { PAGE_SHIM_PROTOCOL, type ChatTarget } from "./bridgeProtocol";
import {
  AnswerNotDetectedError,
  AnswerTimeoutError,
  AttachmentUploadFailedError,
  CancelledError,
  STOPPED_IN_PAGE_MESSAGE,
  BadTargetError,
  ChatUrlMissingError,
  ComposerNotFoundError,
  RateLimitedError,
  SendButtonNotFoundError,
  PreviousAnswerRunningError,
  StopButtonNotFoundError,
  TargetMismatchError,
  TargetNotReachedError,
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

// Wake / put to sleep the MAIN-world pageShim (stream tee, stop-request
// suppression, visibility spoof). It stays dormant outside bridge requests
// so the user's own ChatGPT session behaves normally, Stop button included.
// `ttlMs` is a safety net in case we never get to switch it off. Resolves
// once the shim acknowledged, so the prompt is never submitted before the
// stream tee is armed (500 ms cap if the shim is missing), with the shim's
// `epoch`: switching on starts a new one, and only reports carrying it
// belong to this request (see `shimReport`). Null without an acknowledged
// epoch — no shim, one older than epochs — and for a shim of another
// protocol version (PAGE_SHIM_PROTOCOL; one line says so): a tab keeps the
// shim it loaded across an extension update. `capture` (debug aid, "on"
// only) has the shim keep this epoch's answer streams for
// `takeStreamCapture`; a shim that can do that acknowledges it.
export function setPageShimActive(active: boolean, ttlMs = 0, capture = false): Promise<number | null> {
  return new Promise((resolve) => {
    const onAck = (ev: MessageEvent): void => {
      if (ev.source !== window) return;
      const d = ev.data as Record<string, unknown> | null;
      if (!d || d.__cgptBridgeAck !== true || d.active !== active) return;
      if (typeof d.epoch !== "number") {
        done(null);
        return;
      }
      // Without `v`: version 1, the first with epochs.
      const version = typeof d.v === "number" ? d.v : 1;
      if (version !== PAGE_SHIM_PROTOCOL) {
        if (active) {
          console.warn(
            `[cgpt-bridge] page shim protocol ${version} is not ${PAGE_SHIM_PROTOCOL} ` +
              "(the tab kept it across an extension update): it is not used, the answer is read " +
              "from the page; reload the tab to update it",
          );
        }
        done(null);
        return;
      }
      captureEpoch = active && capture && d.capture === true ? d.epoch : null;
      done(d.epoch);
    };
    const timer = setTimeout(() => done(null), 500);
    const done = (epoch: number | null): void => {
      clearTimeout(timer);
      window.removeEventListener("message", onAck);
      resolve(epoch);
    };
    window.addEventListener("message", onAck);
    try {
      window.postMessage(
        { __cgptBridgeControl: true, active, ttlMs, ...(active && capture ? { capture: true } : {}) },
        window.location.origin,
      );
    } catch {
      done(null);
    }
  });
}

// How long the shim gets to hand over a capture.
const CAPTURE_TAKE_MS = 500;

// The epoch whose capture the shim acknowledged; null when none was asked
// for, or the shim predates captures.
let captureEpoch: number | null = null;

// The raw answer streams the shim kept for `epoch` (see setPageShimActive's
// `capture`), handed over once; null when it kept none, or a shim without
// capture does not answer.
export function takeStreamCapture(epoch: number, waitMs = CAPTURE_TAKE_MS): Promise<string | null> {
  if (captureEpoch !== epoch) {
    console.warn("[cgpt-bridge] page script too old for capture (reload the tab): no capture, no wait");
    return Promise.resolve(null);
  }
  return new Promise((resolve) => {
    const onReply = (ev: MessageEvent): void => {
      if (ev.source !== window) return;
      const d = ev.data as Record<string, unknown> | null;
      if (!d || typeof d !== "object" || d.__cgptBridgeCapture !== true || d.epoch !== epoch) return;
      done(typeof d.raw === "string" ? d.raw : null);
    };
    const timer = setTimeout(() => done(null), waitMs);
    const done = (raw: string | null): void => {
      clearTimeout(timer);
      window.removeEventListener("message", onReply);
      resolve(raw);
    };
    window.addEventListener("message", onReply);
    try {
      window.postMessage({ __cgptBridgeCaptureTake: true, epoch }, window.location.origin);
    } catch {
      done(null);
    }
  });
}

// One `__cgptBridge` message of the pageShim (kind, text, epoch, …).
export type ShimReport = Record<string, unknown>;

// The pageShim report in `ev` if it belongs to `epoch` (as acknowledged by
// `setPageShimActive`), else null: reports of a stream started for an
// earlier request, and reports without an epoch (a shim older than the
// content script), are not ours. With no epoch the answer comes from the
// DOM alone.
function shimReport(ev: MessageEvent, epoch: number | null): ShimReport | null {
  if (epoch === null || ev.source !== window) return null;
  const d = ev.data as Record<string, unknown> | null;
  if (!d || typeof d !== "object" || d.__cgptBridge !== true) return null;
  return d.epoch === epoch ? d : null;
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

export type UiProfile = "account" | "octane" | "classic" | "unknown";

export function detectProfile(): UiProfile {
  if (document.querySelector("[data-composer-markdown], [data-talvt-turn-state]")) {
    return "account";
  }
  if (document.querySelector("button[data-composer-submit], [data-composer-editor-host]")) {
    return "octane";
  }
  if (document.querySelector("#prompt-textarea, [data-message-author-role]")) {
    return "classic";
  }
  return "unknown";
}

// Selectors are intentionally semantic-first. ChatGPT class names are not
// stable and must never be hard-coded here.
const COMPOSER_SELECTORS: readonly string[] = [
  "div.ProseMirror[contenteditable=\"true\"][role=\"textbox\"][data-composer-markdown]",
  "div[contenteditable=\"true\"][role=\"textbox\"][data-composer-markdown]",
  "[data-composer-editor-host] [contenteditable=\"true\"]",
  "[data-composer-editor-host] textarea",
  "textarea[name=\"prompt\"]",
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

// Localized aria-label fallbacks stay last: they only work for en/ru UIs.
const SEND_BUTTON_SELECTORS: readonly string[] = [
  "button[data-composer-submit]",
  "button[data-testid=\"send-button\"]",
  "button[data-testid=\"fruitjuice-send-button\"]",
  "button[data-testid*=\"send\"]",
  "button[aria-label*=\"Send\" i]",
  "button[aria-label*=\"Отправ\" i]",
  "main form button[type=\"submit\"]",
];

const STOP_BUTTON_SELECTORS: readonly string[] = [
  "button[data-testid=\"stop-button\"]",
  "button[aria-label*=\"Stop\" i]",
  "button[aria-label*=\"Останов\" i]",
  "button[aria-label*=\"Перестать\" i]",
];

// aria-label of the button we clicked to submit. On the "octane" markup the
// same button turns into Stop while streaming, so a changed label is a
// language-independent "generating" signal there. ("account" swaps in a
// separate Stop button instead; see the inventory at the top.)
let submittedLabel: string | null = null;

function isUsableButton(btn: HTMLButtonElement): boolean {
  return (
    !btn.disabled &&
    !btn.hidden &&
    btn.getAttribute("aria-disabled") !== "true" &&
    !btn.hasAttribute("data-visually-disabled") &&
    isVisible(btn)
  );
}

// The submit button that belongs to the composer, if the composer sits in a
// <form>. Present on every known markup; label-agnostic.
function composerFormSubmit(): HTMLButtonElement | null {
  const form = findComposer()?.closest("form");
  return form?.querySelector<HTMLButtonElement>("button[type=\"submit\"]") ?? null;
}

function looksLikeStop(btn: HTMLButtonElement): boolean {
  const label = btn.getAttribute("aria-label");
  const stopLabel = btn.dataset["stopLabel"];
  if (stopLabel && label === stopLabel) return true;
  return btn.dataset["testid"] === "stop-button";
}

function findSendButton(): HTMLButtonElement | null {
  const formBtn = composerFormSubmit();
  if (formBtn && isUsableButton(formBtn) && !looksLikeStop(formBtn)) {
    return formBtn;
  }
  for (const selector of SEND_BUTTON_SELECTORS) {
    const btn = document.querySelector<HTMLButtonElement>(selector);
    if (btn && isUsableButton(btn) && !looksLikeStop(btn)) {
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
    setNativeValue(el, text);
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

function setNativeValue(el: HTMLTextAreaElement | HTMLInputElement, value: string): void {
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

// ─── attachments ─────────────────────────────────────────────────────────
// Not verified live (see the inventory at the top): every probe below is a
// guess at semantic markup, kept here so that a fix is a one-file change.

// The remove button of an attachment tile, one per file.
const ATTACHMENT_REMOVE_SELECTOR = [
  "button[aria-label^=\"Remove file\" i]",
  "button[aria-label^=\"Remove attachment\" i]",
  "button[aria-label^=\"Удалить файл\" i]",
  "button[aria-label^=\"Удалить вложение\" i]",
  "button[data-testid*=\"remove-file\"]",
].join(",");
// The preview of an image that is being (or has been) attached.
const ATTACHMENT_PREVIEW_SELECTOR = "img[src^=\"blob:\"]";
// An upload still running in the composer.
const ATTACHMENT_BUSY_SELECTOR = "[role=\"progressbar\"], [aria-busy=\"true\"]";
const FILE_INPUT_SELECTOR = "input[type=\"file\"]";
// The page shows no change for this long after the last tile came: the
// uploads have settled.
const ATTACHMENT_SETTLE_MS = 500;
// How long removing leftover files may take.
const ATTACHMENT_CLEAR_MS = 3_000;

// The composer's own form, where its tiles and its file input are. Never the
// whole page: a project page has its own file upload and "Remove file"
// buttons. Null without one (files are then not attached at all).
function attachmentArea(): HTMLFormElement | null {
  return findComposer()?.closest("form") ?? null;
}

// Files the composer holds now: the larger of the two counts, since a tile
// may show its remove button only after its upload ended. 0 without the
// composer's form.
export function composerAttachmentCount(): number {
  const area = attachmentArea();
  if (area === null) return 0;
  return Math.max(
    area.querySelectorAll(ATTACHMENT_REMOVE_SELECTOR).length,
    area.querySelectorAll(ATTACHMENT_PREVIEW_SELECTOR).length,
  );
}

// An upload still running in the composer.
export function composerUploading(): boolean {
  return attachmentArea()?.querySelector(ATTACHMENT_BUSY_SELECTOR) != null;
}

// Whether `input` takes a file of `mime` named `name` (its `accept`: empty,
// `image/*`, the type itself or the extension).
function accepts(input: HTMLInputElement, mime: string, name: string): boolean {
  const accept = (input.getAttribute("accept") ?? "").trim();
  if (accept === "") return true;
  const lower = name.toLowerCase();
  return accept.split(",").some((token) => {
    const t = token.trim().toLowerCase();
    if (t === "*" || t === "*/*" || t === mime) return true;
    if (t.endsWith("/*")) return mime.startsWith(t.slice(0, -1));
    return t.startsWith(".") && lower.endsWith(t);
  });
}

// A file input of the composer's own form that takes every file.
function findFileInput(area: HTMLFormElement, files: readonly File[]): HTMLInputElement | null {
  const inputs = Array.from(area.querySelectorAll<HTMLInputElement>(FILE_INPUT_SELECTOR));
  return (
    inputs.find((input) => !input.disabled && files.every((f) => accepts(input, f.type, f.name))) ?? null
  );
}

// Remove the files the composer holds (a failed ask's leftovers, which the
// next Send would carry), pressing each tile's remove button. Best effort,
// bounded; true when none is left.
export async function clearComposerAttachments(): Promise<boolean> {
  const end = Date.now() + ATTACHMENT_CLEAR_MS;
  while (composerAttachmentCount() > 0 && Date.now() < end) {
    const button = attachmentArea()?.querySelector<HTMLButtonElement>(ATTACHMENT_REMOVE_SELECTOR) ?? null;
    if (button === null) return false;
    const before = composerAttachmentCount();
    button.click();
    while (composerAttachmentCount() >= before && Date.now() < end) await sleep(50);
  }
  return composerAttachmentCount() === 0;
}

export interface AttachOptions {
  // How long the uploads may take in all.
  timeoutMs: number;
  // Run on every poll; may throw to abort (a Stop, the deadline, a page that
  // left its chat). The files are then removed again.
  check?: () => void;
}

// Put `files` into the composer and wait until every one has uploaded: a
// tile per file and no upload running, unchanged for a moment. Fails with
// AttachmentUploadFailedError when that does not happen in time; nothing is
// sent either way, and the files that did arrive are removed again so the
// next Send does not carry them. Call with an empty composer (no leftovers,
// see clearComposerAttachments) and before typing the text.
export async function attachFiles(files: readonly File[], options: AttachOptions): Promise<void> {
  if (files.length === 0) return;
  const check = options.check ?? (() => {});
  const composer = findComposer();
  if (!composer) throw new ComposerNotFoundError();
  const area = attachmentArea();
  if (area === null) {
    throw new AttachmentUploadFailedError(
      "The ChatGPT composer sits in no form of its own to attach files to; the prompt was not sent.",
    );
  }
  const before = composerAttachmentCount();
  const want = before + files.length;
  try {
    check();
    insertFiles(area, composer, files);
    const end = Date.now() + options.timeoutMs;
    let quietSince = 0;
    let shown = before;
    for (;;) {
      check();
      const count = composerAttachmentCount();
      const ready = count >= want && !composerUploading();
      if (!ready || count !== shown) {
        quietSince = 0;
      } else if (quietSince === 0) {
        quietSince = Date.now();
      } else if (Date.now() - quietSince >= ATTACHMENT_SETTLE_MS) {
        return;
      }
      shown = count;
      if (Date.now() >= end) {
        throw new AttachmentUploadFailedError(
          count < want
            ? `ChatGPT showed ${count - before} of ${files.length} attached files within ` +
              `${Math.round(options.timeoutMs / 1000)} s (upload refused or the attach markup changed); ` +
              "the prompt was not sent."
            : `The files did not finish uploading within ${Math.round(options.timeoutMs / 1000)} s; ` +
              "the prompt was not sent.",
        );
      }
      await sleep(100);
    }
  } catch (err) {
    await clearComposerAttachments().catch(() => false);
    throw err;
  }
}

// The file input of the composer's form when there is one that takes the
// files, else a paste of the files on the composer. Never an input elsewhere
// in the page (a project's own file upload, say).
function insertFiles(area: HTMLFormElement, composer: HTMLElement, files: readonly File[]): void {
  const data = new DataTransfer();
  for (const file of files) data.items.add(file);
  const input = findFileInput(area, files);
  if (input !== null) {
    input.files = data.files;
    input.dispatchEvent(new Event("input", { bubbles: true }));
    input.dispatchEvent(new Event("change", { bubbles: true }));
    return;
  }
  if (typeof ClipboardEvent !== "function") {
    throw new AttachmentUploadFailedError(
      "The ChatGPT page has no file input for the composer; the prompt was not sent.",
    );
  }
  composer.focus();
  composer.dispatchEvent(
    new ClipboardEvent("paste", { bubbles: true, cancelable: true, clipboardData: data }),
  );
}

// `beforeSend` runs right before the click (or the Enter fallback); it may
// throw to abort without sending — the last moment to check for a Stop or a
// page that left the target chat. `shimEpoch` is the pageShim's epoch for
// this request (`setPageShimActive`). `deadline` (epoch ms) is the ask's: the
// Enter fallback's wait for a send sign ends before it. Resolves with that
// epoch's reports posted since the click, in order: a short (say, empty)
// answer's stream may end before the send sign is seen, and its reports
// belong to the answer wait (`WaitForAnswerOptions.earlyShimReports`).
// `sendBy` (epoch ms): the last moment the prompt may still be sent (the
// caller's checkpoint refuses later); the wait for Send does not run past it.
export async function submit(
  beforeSend: () => void = () => {},
  shimEpoch: number | null = null,
  deadline = Infinity,
  sendBy = Infinity,
): Promise<ShimReport[]> {
  // The page shows Send once it has taken the typed text. That can take long
  // in a window the browser slows down (live 02.11, Windows: a gateway ask
  // found no Send within 5 s, and Send was there a little later), so the
  // wait is long, but never past `sendBy`.
  const buttonWaitMs = Math.min(
    SEND_BUTTON_WAIT_MS,
    Math.max(SEND_BUTTON_MIN_WAIT_MS, sendBy - Date.now()),
  );
  const button = await waitForSendButton(buttonWaitMs);
  if (button) {
    beforeSend();
    submittedLabel = button.getAttribute("aria-label");
    const watch = watchSend(shimEpoch);
    try {
      button.click();
      await resendWhileStillInComposer(watch, beforeSend);
    } finally {
      watch.dispose();
    }
    return watch.reports;
  }
  submittedLabel = null;

  const composer = findComposer();
  if (!composer) throw new SendButtonNotFoundError();
  beforeSend();

  // Last-resort fallback: synthesize Enter on the composer. This is not the
  // primary path because the page may treat Enter differently in some modes.
  const keydown = new KeyboardEvent("keydown", {
    key: "Enter",
    code: "Enter",
    bubbles: true,
    cancelable: true,
  });
  const watch = watchSend(shimEpoch);
  try {
    const accepted = composer.dispatchEvent(keydown);
    // If keydown was not cancelled, no submission likely occurred.
    if (accepted) {
      throw new SendButtonNotFoundError(
        `Send button not found within ${Math.round(buttonWaitMs / 1000)} s and the Enter-key fallback ` +
          `was not consumed by the page; the prompt was not sent. ${SLOW_WINDOW_HINT}`,
      );
    }
    // A consumed Enter is not a send either (an editor consumes it while it
    // cannot send): it counts only with a sign that the prompt went out,
    // given as long as a click with its retries, but ending before the
    // ask's deadline so that this error reaches the caller (the host gives
    // up on a silent extension 5 s past it). The page may clear the composer
    // late, so no sign does not prove nothing was sent.
    const signWaitMs = Math.min(
      SEND_CONFIRM_MS * (SEND_RETRIES + 1),
      Math.max(0, deadline - SEND_SIGN_DEADLINE_MARGIN_MS - Date.now()),
    );
    if (!(await waitForPage(watch.sent, signWaitMs))) {
      throw new SendButtonNotFoundError(
        `Send button not found within ${Math.round(buttonWaitMs / 1000)} s; the Enter-key fallback ` +
          "shows no sign the prompt was sent. It may still have been sent: check the ChatGPT tab " +
          `before asking again. ${SLOW_WINDOW_HINT}`,
      );
    }
  } finally {
    watch.dispose();
  }
  return watch.reports;
}

// Live (26.10.2026): a click on Send right after a 20 000-character prompt
// was dropped: the text stayed in the composer and no reply came. Send is
// pressed again only while nothing shows that the click went through; the
// text still being in the composer alone is not enough, since the page may
// clear it late.
async function resendWhileStillInComposer(watch: SendWatch, beforeSend: () => void): Promise<void> {
  for (let attempt = 0; attempt < SEND_RETRIES; attempt++) {
    const sent = await waitForPage(watch.sent, SEND_CONFIRM_MS);
    if (sent) return;
    // A 429 on the answer stream is the page's answer to the click, even if
    // the prompt is still in the composer: pressing Send again changes nothing.
    if (watch.reports.some(isRateLimitSignal)) return;
    const again = findSendButton();
    if (!again) return;
    beforeSend();
    again.click();
  }
}

interface SendWatch {
  sent(): boolean;
  // The `shimEpoch` reports seen so far, in order.
  readonly reports: ShimReport[];
  dispose(): void;
}

// Signs, taken against the moment of the first click, that the prompt went
// out: the composer emptied, the page started generating (a generation
// already running at that moment counts only once it has ended), the address
// changed (a new chat moves from `/` to `/c/…` on submit; in any case the
// composer we typed into is not there any more), a user turn was added, or
// the answer stream of this request started (pageShim `sse-start` in
// `shimEpoch`).
function watchSend(shimEpoch: number | null): SendWatch {
  const path = location.pathname;
  const turns = userTurnCount();
  let idleSeen = !isGenerating();
  let streamStarted = false;
  const reports: ShimReport[] = [];
  const onShimMessage = (ev: MessageEvent): void => {
    const d = shimReport(ev, shimEpoch);
    if (d === null) return;
    reports.push(d);
    if (d.kind === "sse-start") streamStarted = true;
  };
  window.addEventListener("message", onShimMessage);
  return {
    reports,
    sent: () => {
      if (streamStarted || location.pathname !== path || userTurnCount() > turns) return true;
      const composer = findComposer();
      if (composer === null || composerText(composer).length === 0) return true;
      const generating = isGenerating();
      if (!generating) idleSeen = true;
      return generating && idleSeen;
    },
    dispose: () => window.removeEventListener("message", onShimMessage),
  };
}

// The account markup's user message (see the inventory at the top).
const USER_BUBBLE_SELECTOR = "[data-user-message-bubble]";

// A user message on any markup: the account bubble, the octane and classic
// user messages. A page mixing markups (A/B) shows some of each.
const USER_MESSAGE_SELECTOR = [
  USER_BUBBLE_SELECTOR,
  "[data-message-role=\"user\"]",
  "[data-message-author-role=\"user\"]",
].join(",");

// Of `nodes` in document order, those inside none of the others (an
// ancestor comes before its descendants, so one look back is enough).
function outermost(nodes: readonly HTMLElement[]): HTMLElement[] {
  const out: HTMLElement[] = [];
  for (const node of nodes) {
    const prev = out[out.length - 1];
    if (prev !== undefined && prev.contains(node)) continue;
    out.push(node);
  }
  return out;
}

function userMessages(): HTMLElement[] {
  return outermost(Array.from(document.querySelectorAll<HTMLElement>(USER_MESSAGE_SELECTOR)));
}

// User messages on every markup at once (one that carries two marks counts
// once). Where none shows, account turns (user bubble + answer) are counted
// instead: on 2026-10-28 there may be one per message, so a page whose
// bubbles stop matching would count each exchange twice.
export function userTurnCount(): number {
  const count = userMessages().length;
  return count > 0 ? count : document.querySelectorAll(TURN_STATE_SELECTOR).length;
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

// How long the page gets to show Send after the prompt is typed: at most
// SEND_BUTTON_WAIT_MS, and at least SEND_BUTTON_MIN_WAIT_MS however close
// the deadline is.
const SEND_BUTTON_WAIT_MS = 20_000;
const SEND_BUTTON_MIN_WAIT_MS = 5_000;
// What a Send that does not show usually means.
const SLOW_WINDOW_HINT =
  "A ChatGPT window that is minimized or fully covered by other windows can be slowed " +
  "down by the browser (on Windows above all): keep it at least partly visible.";

// How long a Send click gets to empty the composer, and how often it is repeated.
const SEND_CONFIRM_MS = 4_000;
const SEND_RETRIES = 2;
// The Enter fallback's error is due this long before the ask's deadline.
const SEND_SIGN_DEADLINE_MARGIN_MS = 1_000;

// One selector per profile; querySelectorAll returns the union in document
// order, so the last match is the newest answer whichever markup is live.
// "account" carries the answer's id on containers that may nest (one answer
// each, see assistantNodes); the 2026-10-23 variant had the id on the
// `[data-markdown-copy]` element itself.
const ASSISTANT_MESSAGE_SELECTOR = [
  "[data-dil-source-message-id]",
  "[data-chatgpt-selection-message-id]",
  "[data-message-role=\"assistant\"]",
  "[data-message-author-role=\"assistant\"]",
].join(",");
// The answer's text: "account" `[data-markdown-copy]` (its `="exclude"`
// parts, such as a code block header, are not it; see isMarkdownRoot),
// octane, classic.
const MARKDOWN_SELECTOR = "[data-markdown-copy], [data-assistant-markdown-root], .markdown";
const COPY_EXCLUDED_SELECTOR = "[data-markdown-copy=\"exclude\"]";
// Answers of the octane and classic markups, read whole when they hold no
// markdown container. An "account" id container is an answer only with one.
const ROLE_ANSWER_SELECTOR = "[data-message-role=\"assistant\"], [data-message-author-role=\"assistant\"]";
// Never part of the answer text: screen-reader labels and model reasoning.
const NON_CONTENT_SELECTOR = [
  "h4[data-conversation-role]",
  // "account": UI chrome the page itself leaves out of "copy message".
  "[data-markdown-copy=\"exclude\"]",
  // "account": source citations render as a badge inside a popover trigger.
  "[data-d-component=\"popover-trigger\"]:has([data-d-component=\"badge\"])",
  "[data-assistant-reasoning-summary]",
  "[data-assistant-reasoning-transient]",
].join(",");
const TURN_STATE_SELECTOR = "[data-talvt-turn-state]";
const PROTOCOL_LANG_RE = /cgpt-(?:agent-response|command-result)-v\d+/i;
// The gateway's tool-call label. Prose and inline code mention it often, so
// it counts only as the whole label of the block's own header.
// `tool_calls`, alone or with attributes ChatGPT sometimes adds to the info
// string (live 05.11: `tool_calls id="me-review-prep"`); the gateway reads
// the first word as well.
const TOOL_CALLS_LANG_RE = /^tool_calls(?:\s+[A-Za-z_][\w-]*\s*=\s*(?:"[^"]*"|'[^']*'|[^\s"']+))*$/i;

/** The fence label for a block whose header reads `header`, if it is one the
 * bridge parses: `cgpt-*-v<N>` anywhere in it, or `tool_calls` (maybe with
 * `key="value"` attributes after it). */
export function protocolLanguageOf(header: string): string | null {
  const text = header.trim();
  const m = text.match(PROTOCOL_LANG_RE);
  if (m) return m[0].toLowerCase();
  return TOOL_CALLS_LANG_RE.test(text) ? "tool_calls" : null;
}

/** A backtick fence longer than any backtick run that starts a line of
 * `body`, so code holding ``` lines stays one block. */
export function fenceFor(body: string): string {
  let longest = 0;
  for (const m of body.matchAll(/^[ \t]*(`+)/gm)) {
    longest = Math.max(longest, (m[1] ?? "").length);
  }
  return "`".repeat(Math.max(3, longest + 1));
}

// One node per answer, in document order: of nested matches only the
// outermost, never one inside or around a user message, and an "account" id
// container only if it holds the answer's markdown — the id attributes may
// also mark a reasoning recap, a search block or a user message.
function assistantNodes(): HTMLElement[] {
  return outermost(
    Array.from(document.querySelectorAll<HTMLElement>(ASSISTANT_MESSAGE_SELECTOR)).filter(
      (node) =>
        node.closest(USER_BUBBLE_SELECTOR) === null &&
        node.querySelector(USER_BUBBLE_SELECTOR) === null &&
        (node.matches(ROLE_ANSWER_SELECTOR) || markdownRootsOf(node).length > 0),
    ),
  );
}

// The newest answer, if it comes after the newest user message: an answer
// above it belongs to an earlier exchange, whatever its id.
function lastAssistantNode(): HTMLElement | null {
  const nodes = assistantNodes();
  const last = nodes[nodes.length - 1];
  if (last === undefined) return null;
  const users = userMessages();
  const user = users[users.length - 1];
  if (user !== undefined && (user.compareDocumentPosition(last) & Node.DOCUMENT_POSITION_FOLLOWING) === 0) {
    return null;
  }
  return last;
}

function messageIdOf(node: HTMLElement): string | null {
  return (
    node.dataset["messageId"] ??
    node.dataset["dilSourceMessageId"] ??
    node.dataset["chatgptSelectionMessageId"] ??
    node.dataset["requestMessageId"] ??
    null
  );
}

export function getLastAssistantMessageId(): string | null {
  const last = lastAssistantNode();
  return last ? messageIdOf(last) : null;
}

// What "the newest answer" looked like right before we submitted. A reply is
// new when there are more answers than before, or the newest one carries a
// different message id. Ids alone are not enough: the octane markup has no
// reliable id attribute.
export interface AnswerBaseline {
  count: number;
  messageId: string | null;
}

export function captureAnswerBaseline(): AnswerBaseline {
  return { count: assistantNodes().length, messageId: getLastAssistantMessageId() };
}

function isNewAnswer(node: HTMLElement, baseline: AnswerBaseline): boolean {
  if (assistantNodes().length > baseline.count) return true;
  const id = messageIdOf(node);
  return id !== null && id !== baseline.messageId;
}

function isMarkdownRoot(el: HTMLElement): boolean {
  return el.matches(MARKDOWN_SELECTOR) && !el.matches(COPY_EXCLUDED_SELECTOR);
}

// The rendered markdown containers of an answer node: the node itself if it
// is one (the 2026-10-23 variant), else the outermost ones inside it.
function markdownRootsOf(node: HTMLElement): HTMLElement[] {
  if (isMarkdownRoot(node)) return [node];
  return outermost(Array.from(node.querySelectorAll<HTMLElement>(MARKDOWN_SELECTOR)).filter(isMarkdownRoot));
}

// The answer's markdown container: the last one in the node. The answer is
// the final channel, which the page streams after any reasoning recap or
// search step of the same reply (2026-10-28), so a container above it is
// not the answer.
function markdownRootOf(node: HTMLElement): HTMLElement | null {
  const roots = markdownRootsOf(node);
  return roots[roots.length - 1] ?? null;
}

function readAssistantText(node: HTMLElement): string | null {
  // Prefer the rendered markdown container so we drop the sr-only "ChatGPT
  // said:" label and any non-content chrome attached to the message wrapper.
  const md = markdownRootOf(node);
  const source = md ?? node;

  // ChatGPT renders ```info … ``` fences as styled panels without the literal
  // backticks. The Rust agent parser keys on exactly those backticks + info
  // string, so we walk the DOM and rebuild the source markdown for code
  // blocks. Other elements collapse to their visible text.
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

  if (el.matches(NON_CONTENT_SELECTOR)) return;
  // Classic / CodeMirror markup: the code block's header (label, Copy button)
  // is chrome; its label goes on the fence (findCodeBlockLanguage).
  if (tag === "DIV" && isCodeHeader(el)) return;

  // "account" markup: the block header holds the display-cased language
  // label as a plain leaf element next to the <pre>.
  if (el.dataset["codeBlock"] === "true") {
    const source =
      el.querySelector<HTMLElement>("[data-code-block-source]") ??
      el.querySelector<HTMLElement>("pre");
    if (source) {
      const code = source.querySelector<HTMLElement>("code") ?? source;
      emitFence(findHeaderLanguage(el, source), extractCodeText(code), out);
      return;
    }
  }

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
  const fence = fenceFor(body);
  out.push("\n" + fence + lang + "\n");
  out.push(body);
  if (!body.endsWith("\n")) out.push("\n");
  out.push(fence + "\n");
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

// Locate the language label for a code block. ChatGPT puts it in the block's
// own header (see codeHeaderOf). A header label that is one token (HEADER_TOKEN_RE)
// is the fence's info string: a protocol label normalized (`tool_calls` only as
// the exact label), any other language lower-cased. Anything else gives no info
// string. Text outside that header (prose before the block, another block's
// header or code) never labels the block, and a block with no header has none.
function findCodeBlockLanguage(block: HTMLElement): string {
  const header = codeHeaderOf(block);
  if (!header) return "";
  return infoStringOf(headerLabel(header));
}

// The fence info string for a header label: the label if it is one token
// (HEADER_TOKEN_RE; a protocol label normalized), else none.
function infoStringOf(label: string): string {
  // A protocol label first: `tool_calls id="…"` is no single token.
  const protocol = protocolLanguageOf(label);
  if (protocol !== null) return protocol;
  if (!HEADER_TOKEN_RE.test(label)) return "";
  return label.toLowerCase();
}

const CODE_BLOCK_SELECTOR = "pre, .cm-editor, #code-block-viewer";
// A header label usable as a fence info string: one short token, no backtick.
const HEADER_TOKEN_RE = /^[A-Za-z0-9+#._-]{1,32}$/;
// What a header never holds outside its buttons: prose, rich text, media,
// math, inline code or a code block. When in doubt, the text stays content.
const NOT_IN_HEADER_SELECTOR = [
  "p, li, ul, ol, blockquote, h1, h2, h3, h4, h5, h6, table, details",
  "a, strong, em, b, i, img, hr, math, .katex",
  `code, ${CODE_BLOCK_SELECTOR}`,
].join(", ");
const COPY_BUTTON_RE = /copy|копир/i;

function isOtherCode(el: Element, code: HTMLElement): boolean {
  return !el.contains(code) && !code.contains(el);
}

function holdsOtherCode(container: HTMLElement, code: HTMLElement): boolean {
  return Array.from(container.querySelectorAll(CODE_BLOCK_SELECTOR)).some((el) => isOtherCode(el, code));
}

// The nearest sibling of `node` in direction `step` (-1 before, 1 after) that
// is an element or visible text; null if there is none.
function adjacentNode(node: Node, step: -1 | 1): Node | null {
  const siblings = Array.from(node.parentElement?.childNodes ?? []);
  for (let i = siblings.indexOf(node as ChildNode) + step; i >= 0 && i < siblings.length; i += step) {
    const s = siblings[i]!;
    if (s.nodeType === Node.ELEMENT_NODE) return s;
    if (s.nodeType === Node.TEXT_NODE && (s.textContent ?? "").trim() !== "") return s;
  }
  return null;
}

function outsideButtons(el: HTMLElement, selector: string): HTMLElement[] {
  return Array.from(el.querySelectorAll<HTMLElement>(selector)).filter((d) => !d.closest("button"));
}

// A header looks like one: a <div> with nothing of NOT_IN_HEADER_SELECTOR
// outside its buttons, and either a Copy button or, with no nested <div>, a
// text (buttons aside) that is one short token: the label alone.
function isHeaderShaped(el: HTMLElement): boolean {
  if (el.tagName !== "DIV" || outsideButtons(el, NOT_IN_HEADER_SELECTOR).length > 0) return false;
  const copy = Array.from(el.querySelectorAll<HTMLElement>("button")).some((b) =>
    COPY_BUTTON_RE.test(`${b.textContent ?? ""} ${b.getAttribute("aria-label") ?? ""}`),
  );
  if (copy) return true;
  return outsideButtons(el, "div").length === 0 && HEADER_TOKEN_RE.test(headerLabel(el));
}

// The header of a code block (classic and CodeMirror markup): a header-shaped
// <div> (isHeaderShaped) right before the block or before a wrapper of the
// block that holds no other code block. Prose, inline code, bare text and a
// <div> of answer content are never a header.
function codeHeaderOf(block: HTMLElement): HTMLElement | null {
  // The block itself, then up to 5 wrappers (the reach the label search had).
  let cur: HTMLElement = block;
  for (let depth = 0; depth <= 5; depth++) {
    const prev = adjacentNode(cur, -1);
    if (prev) {
      return prev.nodeType === Node.ELEMENT_NODE && isHeaderShaped(prev as HTMLElement) ? (prev as HTMLElement) : null;
    }
    const parent = cur.parentElement;
    if (!parent || isMarkdownRoot(parent) || parent.matches(ASSISTANT_MESSAGE_SELECTOR)) return null;
    if (holdsOtherCode(parent, block)) return null;
    cur = parent;
  }
  return null;
}

// Whether `el` is the header of the code block right after it. A header is
// chrome (label, Copy button): its text is not part of the answer.
function isCodeHeader(el: HTMLElement): boolean {
  const next = adjacentNode(el, 1);
  if (next?.nodeType !== Node.ELEMENT_NODE) return false;
  const nextEl = next as HTMLElement;
  const block = nextEl.matches(CODE_BLOCK_SELECTOR) ? nextEl : nextEl.querySelector<HTMLElement>(CODE_BLOCK_SELECTOR);
  return block !== null && codeHeaderOf(block) === el;
}

// The header's text, button labels skipped.
function headerLabel(header: HTMLElement): string {
  const walker = document.createTreeWalker(header, NodeFilter.SHOW_TEXT);
  const texts: string[] = [];
  for (let n = walker.nextNode(); n; n = walker.nextNode()) {
    if (n.parentElement?.closest("button")) continue;
    const text = (n.textContent ?? "").trim();
    if (text.length > 0) texts.push(text);
  }
  return texts.join(" ");
}

function findHeaderLanguage(block: HTMLElement, pre: HTMLElement): string {
  for (const el of Array.from(block.querySelectorAll<HTMLElement>("*"))) {
    if (el.children.length > 0 || pre.contains(el) || el.closest("button")) continue;
    const text = (el.textContent ?? "").trim();
    if (text.length === 0) continue;
    return infoStringOf(text);
  }
  return "";
}

export function getLastAssistantMessage(): string | null {
  const last = lastAssistantNode();
  if (!last) return null;
  return readAssistantText(last);
}

const STREAM_ACTIVE_SELECTOR =
  "[data-scroll-root][data-stream-active], [data-conversation-stream-active], [data-message-streaming]";

export function isGenerating(): boolean {
  return generatingSignal() !== null;
}

/**
 * Which sign says the page is generating, or null: `stream-attr`,
 * `turn-in_progress`, `form-label` or `stop-selector:<aria-label>`. Button
 * signs count only a button `findStopButton()` would press, so a hidden or
 * disabled Stop-like button never reads as "generating".
 */
export function generatingSignal(): string | null {
  // classic: the scroll-root carries `data-stream-active` while streaming.
  // octane: explicit streaming attributes.
  if (document.querySelector(STREAM_ACTIVE_SELECTOR)) return "stream-attr";
  // account: the newest turn reports `in_progress`. (`complete` is not a
  // reliable "done" — it also shows briefly before the reply starts.)
  const turns = document.querySelectorAll<HTMLElement>(TURN_STATE_SELECTOR);
  const lastTurn = turns[turns.length - 1];
  if (lastTurn && lastTurn.dataset["talvtTurnState"] === "in_progress") {
    return "turn-in_progress";
  }
  if (formStopButton()) return "form-label";
  const stop = selectorStopButton();
  if (stop) return `stop-selector:${stop.getAttribute("aria-label") ?? stop.dataset["testid"] ?? ""}`;
  return null;
}

// After a Stop for the ask, how long the page must stay idle before the
// answer wait ends (see `WaitForAnswerOptions.stopped`).
const STOPPED_SETTLE_MS = 1_000;

export interface WaitForAnswerOptions {
  stabilityMs?: number;
  pollIntervalMs?: number;
  // Fail fast when, this long after submit, neither the DOM (new answer or
  // "generating" state) nor the pageShim stream showed any sign of a reply.
  // That almost always means ChatGPT changed its markup.
  noSignalMs?: number;
  // When the DOM says "done" while the pageShim stream is still open, wait
  // this long for the stream's own completion: its text is the source
  // markdown (exact code fences, links), the DOM text is a rendering.
  // Also how long the DOM gets to show an answer after the stream ended
  // without answer text, before the wait fails.
  shimGraceMs?: number;
  // At the deadline, return what was there (a cut-off answer) instead of
  // failing. On for the CLI (historic behaviour); gateway asks turn it off,
  // since a truncated answer would pass for a whole one.
  partialOnTimeout?: boolean;
  // The pageShim's epoch for this request (`setPageShimActive`). Only
  // stream reports carrying it count; without one the DOM alone decides.
  shimEpoch?: number | null;
  // Reports in `shimEpoch` posted before this wait began (`submit`'s
  // result), handled in order right after the first tick, as if they
  // arrived then.
  earlyShimReports?: readonly ShimReport[];
  // Runs on every tick before anything else; a throw ends the wait with
  // that error (the caller's own reasons to stop waiting).
  check?: () => void;
  // True once Stop was pressed for this ask. Its answer then ends where the
  // page stopped it: the stream's abort is not a network cut, and the stable
  // text is taken (live: after a mid-answer Stop the turn reads `complete`,
  // not `cancelled`, so a cut wait would hold the ask to its deadline).
  stopped?: () => boolean;
  // Filled in when the wait ends: the stream's answer text as it last was
  // (null: none, or the stream was cut) and whether the stream completed.
  seen?: AnswerSeen;
}

export interface AnswerSeen {
  streamText: string | null;
  streamDone: boolean;
}

export interface DeadlineSnapshot {
  shimDone: boolean;
  shimText: string | null;
  lastSeenText: string | null;
}

/**
 * Answer to return when the wait hits its deadline, or null for a timeout
 * error. A stream that completed is a whole answer; anything else is cut
 * off and only returned with `partialOnTimeout`.
 */
export function textAtDeadline(s: DeadlineSnapshot, partialOnTimeout: boolean): string | null {
  // The shim saw the whole SSE stream but the DOM never rendered it
  // (typical on hidden tabs with throttled React).
  if (s.shimDone && s.shimText) return s.shimText;
  if (!partialOnTimeout) return null;
  if (s.shimText && s.shimText.length > 100) return s.shimText;
  return s.lastSeenText || null;
}

export async function waitForNewAnswer(
  baseline: AnswerBaseline,
  timeoutMs = 120_000,
  options: WaitForAnswerOptions = {},
): Promise<string> {
  const stabilityMs = options.stabilityMs ?? 800;
  const pollIntervalMs = options.pollIntervalMs ?? 100;
  const noSignalMs = options.noSignalMs ?? 30_000;
  const shimGraceMs = options.shimGraceMs ?? 3_000;
  const partialOnTimeout = options.partialOnTimeout ?? true;
  const shimEpoch = options.shimEpoch ?? null;
  const check = options.check ?? ((): void => {});
  const stopped = options.stopped ?? ((): boolean => false);

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
    let shimFailed = false;
    let sawSignal = false;
    let domReadyAt = 0;
    // When the stream ended without answer text, or the page last showed
    // "generating" after that; 0 while neither.
    let emptyStreamSince = 0;
    // When the page was first seen idle after a Stop for this ask (0: not yet).
    let stoppedIdleSince = 0;
    // The stream broke off before its end with no HTTP status: a network
    // cut (live 28.10: the server finishes the answer and the page shows all
    // of it once the network is back) or the page aborting its fetch (Stop).
    // Not a failure: from then on the page's own answer alone decides (see
    // `settledAfterCut`), up to the deadline as usual. No stream text counts
    // any more, not even a resumed stream's: it may hold only the rest.
    let streamCut = false;
    // What the page showed when the cut was reported: the new answer's text
    // (null: none yet) and whether its turn read as ended.
    let cutText: string | null = null;
    let cutEnded: boolean | null = null;

    // After a cut, the stable DOM text is the answer only if the page shows
    // its turn finished since the cut: new text, or a turn that ended only
    // since then, and a turn that reads `complete` (without turn states, new
    // text is enough). The partial text shown during an outage, even under
    // `complete`, is not an answer. A Stop's `cancelled` turn ends the wait
    // as before; content.ts fails a gateway ask on it.
    const settledAfterCut = (text: string): boolean => {
      if (lastTurnCancelled() || stopped()) return true;
      const ended = lastTurnEnded();
      if (ended === false) return false;
      return text !== cutText || (ended === true && cutEnded !== true);
    };

    const finish = (err: Error | null, value: string | null): void => {
      if (done) return;
      done = true;
      if (options.seen !== undefined) {
        options.seen.streamText = streamCut ? null : shimText;
        options.seen.streamDone = shimDone && !streamCut;
      }
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
    // stop arriving, this path still resolves the wait. Reports of another
    // request's stream are not looked at (see `shimReport`).
    const onShimReport = (d: ShimReport): void => {
      sawSignal = true;
      if (isRateLimitSignal(d)) {
        finish(new RateLimitedError(), null);
        return;
      }
      const kind = d.kind;
      const text = d.text;
      if (kind === "sse-error") {
        shimFailed = true;
        if (typeof d.status !== "number" && !streamCut) {
          streamCut = true;
          const node = lastAssistantNode();
          cutText = node !== null && isNewAnswer(node, baseline) ? readAssistantText(node) : null;
          cutEnded = lastTurnEnded();
        }
      }
      if (streamCut && (kind === "sse-tokens" || kind === "sse-done")) return;
      // A new stream in this ask: an earlier empty one no longer decides.
      if (kind === "sse-start") emptyStreamSince = 0;
      if (typeof text === "string") {
        shimText = text;
        if (kind === "sse-done") {
          shimDone = true;
          if (text.length > 0) {
            finish(null, text);
          } else if (lastSeenText) {
            finish(null, lastSeenText);
          } else {
            emptyStreamSince = Date.now();
          }
        }
      }
    };
    const onShimMessage = (ev: MessageEvent): void => {
      const d = shimReport(ev, shimEpoch);
      if (d !== null) onShimReport(d);
    };
    window.addEventListener("message", onShimMessage);

    const tick = (): void => {
      if (done) return;
      try {
        check();
      } catch (err) {
        finish(err instanceof Error ? err : new Error(String(err)), null);
        return;
      }
      if (Date.now() - start > timeoutMs) {
        // After a cut, stream text is never the answer (see `streamCut`).
        const last = textAtDeadline(
          { shimDone: shimDone && !streamCut, shimText: streamCut ? null : shimText, lastSeenText },
          partialOnTimeout,
        );
        if (last !== null) {
          finish(null, last);
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
      // A Stop pressed for this ask: once the page has been idle for
      // STOPPED_SETTLE_MS the ask ends, with the text seen so far or as
      // stopped, whatever the page shows. Live 30.10: after a gateway Stop
      // the page read `cancelled` with nothing running, and the ask still
      // waited to its deadline, holding the gateway.
      if (stopped()) {
        if (isGenerating()) {
          stoppedIdleSince = 0;
        } else if (stoppedIdleSince === 0) {
          stoppedIdleSince = Date.now();
        } else if (Date.now() - stoppedIdleSince >= STOPPED_SETTLE_MS) {
          if (lastSeenText) finish(null, lastSeenText);
          else finish(new CancelledError(STOPPED_IN_PAGE_MESSAGE), null);
          return;
        }
      }
      const node = lastAssistantNode();
      const isNewTurn = node !== null && isNewAnswer(node, baseline);
      if (isNewTurn || isGenerating()) sawSignal = true;
      if (!sawSignal && Date.now() - start > noSignalMs) {
        finish(
          new AnswerNotDetectedError(
            `No reply detected ${noSignalMs}ms after submit. ${describeDom()}`,
          ),
          null,
        );
        return;
      }
      if (node !== null && isNewTurn) {
        const currentText = readAssistantText(node);
        if (currentText && currentText === lastSeenText) {
          const settled = !streamCut || settledAfterCut(currentText);
          if (Date.now() - stableSince >= stabilityMs && !isGenerating() && settled) {
            // While the answer stream is open but has produced no text yet
            // (the model is still thinking), the DOM cannot be done: this
            // holds regardless of UI language. Once the stream has text,
            // give it `shimGraceMs` to complete before taking the DOM text.
            if (domReadyAt === 0) domReadyAt = Date.now();
            const streamOpen = shimText !== null && !shimDone && !shimFailed;
            const streamSilent = streamOpen && shimText === "";
            if (!streamSilent && (!streamOpen || Date.now() - domReadyAt >= shimGraceMs)) {
              finish(null, currentText);
              return;
            }
          }
        } else if (currentText) {
          lastSeenText = currentText;
          stableSince = Date.now();
        }
      }
      // The stream ended without answer text (an error inside the stream,
      // moderation, an answer that is not text): unless the DOM shows an
      // answer, none is coming. Give the DOM `shimGraceMs` once the page
      // stopped generating, then fail instead of waiting for the deadline.
      if (emptyStreamSince !== 0 && !shimText && lastSeenText === null) {
        if (isGenerating()) {
          emptyStreamSince = Date.now();
        } else if (Date.now() - emptyStreamSince >= shimGraceMs) {
          finish(
            new AnswerNotDetectedError(
              "answer stream ended without answer text; " + describeDom() + "; check the ChatGPT tab",
            ),
            null,
          );
          return;
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
    // The first tick's `check` (a refused stop, the target) comes before
    // the early reports, as it did before they were kept.
    tick();
    for (const d of options.earlyShimReports ?? []) {
      if (done) break;
      onShimReport(d);
    }
  });
}

// ---------------------------------------------------------------------------
// Stream ↔ page check: the answer the stream gave next to the one the page
// shows. Only a report; the ask's answer stays what it was.
// ---------------------------------------------------------------------------

/** The answer's text without what the page's rendering drops or adds:
 * whitespace, fence lines (with their info string: the page shows the label
 * its own way), list markers, rules and table separators, link targets,
 * backslash escapes and markdown emphasis, code and quote marks. */
export function answerSkeleton(text: string): string {
  return text
    .replace(/^[ \t]*(?:`{3,}|~{3,}).*$/gm, "")
    .replace(/^[ \t]*[-*_:| ]{3,}$/gm, "")
    .replace(/^[ \t]*(?:[-*+]|\d+[.)])[ \t]+/gm, "")
    .replace(/\[([^\]\n]*)\]\((?:[^()\s]|\([^()\s]*\))*\)/g, "$1")
    .replace(/\\([\\`*_{}[\]()#+\-.!>~|])/g, "$1")
    .replace(/[\s`*_#>~|]/g, "");
}

export interface StreamPageCheck {
  streamChars: number;
  pageChars: number;
  mismatch: boolean;
}

/** The stream's answer next to the page's, by their skeletons. */
export function compareStreamAndPage(streamText: string, pageText: string): StreamPageCheck {
  const a = answerSkeleton(streamText);
  const b = answerSkeleton(pageText);
  return { streamChars: a.length, pageChars: b.length, mismatch: a !== b };
}

/**
 * Compare the stream's answer with the new answer the page shows (after
 * `baseline`), read once and at once: no wait is added to the ask. Null
 * (no check, not a mismatch) when the page shows no new answer yet, still
 * generates, or its turn has not ended: in a hidden window the page often
 * lags behind the stream.
 */
export function pageCheckNow(baseline: AnswerBaseline, streamText: string): StreamPageCheck | null {
  const node = lastAssistantNode();
  if (node === null || !isNewAnswer(node, baseline)) return null;
  if (isGenerating() || lastTurnEnded() === false) return null;
  const text = readAssistantText(node);
  return text === null ? null : compareStreamAndPage(streamText, text);
}

// ---------------------------------------------------------------------------
// Chat addressing, Stop and rate limit — for gateway requests, which run in
// the extension's own ChatGPT tab and name the chat they belong to.
// ---------------------------------------------------------------------------

const CHATGPT_ORIGIN = "https://chatgpt.com";
const NEW_CHAT_CYCLE_SELECTOR = "button[data-sidebar-cycle-item]";
const NEW_CHAT_LABEL_RE = /^(new chat|новый чат)$/i;
const SIDEBAR_CHAT_LINK_SELECTOR = "a[href^=\"/c/\"][data-interactive-row-link=\"true\"]";
// An in-page switch (button / sidebar link) that takes longer than this
// falls back to a full page load.
const IN_PAGE_NAV_TIMEOUT_MS = 10_000;
// How long an already open (or freshly loaded) chat may take to render.
const CHAT_RENDER_TIMEOUT_MS = 15_000;
// The rendered chat must hold still this long before the answer baseline is
// taken, otherwise turns that render late would pass for a new answer. A new
// chat opened in-page waits as long before typing.
const CHAT_SETTLE_MS = 500;
// Right after submit the page may not show "generating" yet; a Stop waits
// this long for it before concluding there is nothing to press.
const STOP_START_GRACE_MS = 2_000;

// A project's id, its page and a chat in it (see the inventory at the top).
// The id has a fixed length, which is where the slug after it starts.
const PROJECT_ID_RE = /^g-p-[0-9a-f]{32}$/i;
const PROJECT_PAGE_RE = /^\/g\/(g-p-[0-9a-f]{32})\/project$/i;
const PROJECT_CHAT_RE = /^\/g\/(g-p-[0-9a-f]{32})(?:-[^/]+)?\/c\/([^/]+)$/i;

/** A chat's address read from a path: its project (null outside one) and id. */
interface ChatPath {
  projectId: string | null;
  chatId: string;
}

// `/c/<id>`, or `/g/<project id>[-<slug>]/c/<id>`; null for other pages.
function chatOfPath(pathname: string): ChatPath | null {
  if (pathname.startsWith("/c/") && pathname.length > "/c/".length) {
    return { projectId: null, chatId: pathname.slice("/c/".length) };
  }
  const m = PROJECT_CHAT_RE.exec(pathname);
  return m ? { projectId: (m[1] ?? "").toLowerCase(), chatId: m[2] ?? "" } : null;
}

/** The project whose page (`/g/<id>/project`) is at `pathname`, or null. */
export function projectPageId(pathname: string): string | null {
  const m = PROJECT_PAGE_RE.exec(pathname);
  return m ? (m[1] ?? "").toLowerCase() : null;
}

/**
 * The path of a saved conversation — `/c/<id>`, or `/g/<project>-<slug>/c/<id>`
 * in a ChatGPT project — or null: the temporary `/c/local-chatgpt:<uuid>`
 * path of an unsaved chat, a new chat, other pages.
 */
export function conversationPathFrom(pathname: string): string | null {
  const chat = chatOfPath(pathname);
  if (chat === null || chat.chatId.includes("local-chatgpt")) return null;
  return pathname;
}

/** Same as `conversationPathFrom`, for a full URL; null for other sites. */
export function conversationPathOfUrl(url: string | undefined): string | null {
  if (typeof url !== "string") return null;
  try {
    const parsed = new URL(url);
    return parsed.origin === CHATGPT_ORIGIN ? conversationPathFrom(parsed.pathname) : null;
  } catch {
    return null;
  }
}

/**
 * Whether the page at `pathname` shows the chat of `url`, compared by chat
 * id only (as `chat_key` does): a chat that gets its project prefix later
 * (`/c/<id>`, then `/g/<project>-<slug>/c/<id>`) is the same chat.
 */
export function showsSameChatId(url: string, pathname: string): boolean {
  const path = conversationPathOfUrl(url);
  const want = path === null ? null : chatOfPath(path);
  const got = chatOfPath(pathname);
  return want !== null && got !== null && want.chatId === got.chatId;
}

/**
 * A target as the page reaches it: a new chat may be opened in a ChatGPT
 * project (its `g-p-<hex>` id), by `openTarget`.
 */
export type PageTarget =
  | { kind: "conversation"; url: string }
  | { kind: "new_chat"; projectId?: string };

// Where a new chat of `target` opens: `/`, or its project's page.
function newChatPath(target: { projectId?: string }): string {
  return target.projectId === undefined ? "/" : `/g/${target.projectId}/project`;
}

/**
 * Whether a page at `pathname` shows `target`: the conversation (the same
 * chat in the same project, or outside any; a project's slug may change
 * with its name), or the page of a new chat — `/`, or the project's page —
 * (only true before its first submit — afterwards the URL moves to `/c/…`).
 */
export function targetMatchesPath(target: PageTarget, pathname: string): boolean {
  if (target.kind === "new_chat") return pathname.toLowerCase() === newChatPath(target).toLowerCase();
  const path = conversationPathOfUrl(target.url);
  const want = path === null ? null : chatOfPath(path);
  const got = chatOfPath(pathname);
  return (
    want !== null &&
    got !== null &&
    want.projectId === got.projectId &&
    want.chatId === got.chatId
  );
}

// Whether `pathname` is in project `projectId`: its page or one of its chats.
function inProject(pathname: string, projectId: string): boolean {
  const id = projectId.toLowerCase();
  return projectPageId(pathname) === id || chatOfPath(pathname)?.projectId === id;
}

/** Fail unless the page still shows `target` (checked right before typing and submit). */
export function assertAtTarget(target: PageTarget): void {
  if (!targetMatchesPath(target, location.pathname)) {
    const wanted =
      target.kind === "conversation"
        ? target.url
        : target.projectId === undefined
          ? "a new chat"
          : `a new chat in project ${target.projectId}`;
    throw new TargetMismatchError(
      `The page left the target chat before the prompt was sent: at ${location.pathname}, want ${wanted}.`,
    );
  }
}

// How much of the prompt (as a matchKey) the new chat's user message must hold.
const PROMPT_PREFIX_CHARS = 60;

// Letters and digits only, NFKC-folded and lowercased: what survives the
// page rendering the prompt (markdown, quotes, spacing, labels around it).
function matchKey(text: string): string {
  return text.normalize("NFKC").toLowerCase().replace(/[^\p{L}\p{N}]+/gu, "");
}

/**
 * Fail unless the page shows the chat a prompt was sent to (checked once
 * the answer is in, and right before a Stop is pressed for the turn: the
 * user may have switched the page to another chat after Send). A
 * conversation must still be open. A new chat must hold exactly the one
 * user turn the ask sent, and its user message, where the layout shows one,
 * must hold the start of `prompt` (compared by `matchKey`). `when` names the
 * moment in the error.
 */
export function assertAnsweredInTarget(
  target: PageTarget,
  prompt: string,
  when = "before the answer was read",
): void {
  if (target.kind === "conversation") {
    if (!targetMatchesPath(target, location.pathname)) {
      throw new TargetMismatchError(
        `The page left the target chat ${when}: at ${location.pathname}, want ${target.url}.`,
      );
    }
    return;
  }
  // A new chat in a project stays in it: its page, then one of its chats.
  if (target.projectId !== undefined && !inProject(location.pathname, target.projectId)) {
    throw new TargetMismatchError(
      `The page left project ${target.projectId} ${when}: at ${location.pathname}.`,
    );
  }
  const turns = userTurnCount();
  if (turns !== 1) {
    throw new TargetMismatchError(
      `The new chat holds ${turns} user turns ${when}, want 1: at ${location.pathname}.` +
        (turns > 1
          ? " The bridge window shows another chat (switched by a click?) or a second message was " +
            "typed into it; the prompt was sent, and ChatGPT goes on answering it in its own chat."
          : ""),
    );
  }
  const bubbles = document.querySelectorAll<HTMLElement>(USER_BUBBLE_SELECTOR);
  const bubble = bubbles[bubbles.length - 1];
  if (bubble === undefined) return;
  const want = matchKey(prompt).slice(0, PROMPT_PREFIX_CHARS);
  if (want !== "" && !matchKey(bubble.textContent ?? "").includes(want)) {
    throw new TargetMismatchError(
      `The new chat's user message is not the prompt sent: at ${location.pathname}.`,
    );
  }
}

// The state of the newest turn ("account" markup), or null: the others show none.
function lastTurnState(): string | null {
  const turns = document.querySelectorAll<HTMLElement>(TURN_STATE_SELECTOR);
  return turns[turns.length - 1]?.dataset["talvtTurnState"] ?? null;
}

/** The newest turn ended by a Stop ("account" markup; the others show no such state). */
export function lastTurnCancelled(): boolean {
  return lastTurnState() === "cancelled";
}

/**
 * Whether the newest turn has ended ("account" markup): `complete`, or
 * `cancelled` by a Stop. Any other value counts as not ended — the values a
 * running turn shows are not all known (2026-10-28). Null on a markup that
 * shows no turn state. `complete` also shows briefly before a reply starts,
 * so it means "ended" only for a turn whose answer is already there.
 */
export function lastTurnEnded(): boolean | null {
  const state = lastTurnState();
  if (state === null) return null;
  return state === "complete" || state === "cancelled";
}

/** pageShim message meaning "the answer stream got HTTP 429". */
export function isRateLimitSignal(data: unknown): boolean {
  if (!data || typeof data !== "object") return false;
  const d = data as Record<string, unknown>;
  return d.__cgptBridge === true && d.kind === "sse-error" && d.status === 429;
}

export function currentConversationUrl(): string | null {
  const path = conversationPathFrom(location.pathname);
  return path === null ? null : CHATGPT_ORIGIN + path;
}

// A saved chat at `pathname` where a new chat of `target` lands: outside
// any project (`/c/<id>`), or in its project only.
function savedChatOf(target: PageTarget, pathname: string): string | null {
  const path = conversationPathFrom(pathname);
  if (path === null || target.kind !== "new_chat") return path;
  const projectId = target.projectId?.toLowerCase() ?? null;
  return chatOfPath(path)?.projectId === projectId ? path : null;
}

/**
 * Saved URL of the chat a targeted (gateway) ask went to. Right after the
 * first answer of a new chat the page may still show the temporary path; it
 * waits up to `timeoutMs` for the real one. A chat that never gets a saved
 * address (temporary chat, history off) cannot be addressed again, so the
 * ask fails naming the path. The gateway then reads "no URL" only as an
 * extension that predates targets. A new chat's address must be in its
 * project (`/g/<project>-<slug>/c/<id>`), or outside any (`/c/<id>`).
 */
export async function waitForSavedChatUrl(timeoutMs: number, target: PageTarget): Promise<string> {
  await waitForPage(() => savedChatOf(target, location.pathname) !== null, timeoutMs);
  const path = savedChatOf(target, location.pathname);
  if (path === null) {
    const want =
      target.kind === "new_chat" && target.projectId !== undefined
        ? `chatgpt.com/g/${target.projectId}-…/c/<id>`
        : "chatgpt.com/c/<id>";
    throw new ChatUrlMissingError(
      `The chat got no saved ${want} address within ${timeoutMs} ms after the answer ` +
        `(temporary chat or chat history off?): at ${location.pathname}. ${describeDom()}`,
    );
  }
  return CHATGPT_ORIGIN + path;
}

/** Signed in: a composer is there and it is not the signed-out ("octane") UI. */
export function isLoggedIn(): boolean {
  return findComposer() !== null && detectProfile() !== "octane";
}

/**
 * Whether the page is signed in, given `timeoutMs` to become so: a page
 * still loading may show the signed-out markup for a moment.
 */
export function waitForLoggedIn(timeoutMs: number): Promise<boolean> {
  return waitForPage(isLoggedIn, timeoutMs);
}

/**
 * `ready`: the page shows `target` as reached — a new chat opened in its
 * project carries the project's id — with `warning` when the new chat could
 * not be put into the project asked for and is a plain one. Otherwise the
 * caller loads `url` and asks again.
 */
export type OpenTargetResult =
  | { ready: true; target: PageTarget; warning?: string }
  | { ready: false; url: string };

export interface OpenTargetOptions {
  // Epoch ms; every wait is cut to what is left of it.
  deadline: number;
  // The page was just loaded for this target: do not ask for another load.
  reloaded: boolean;
  // A new chat goes into the ChatGPT project of this name (see openProject).
  project?: string;
}

/**
 * Bring the page to `target` before a prompt is sent. `ready`: the chat is
 * open, rendered and the composer is there. Otherwise an in-page switch was
 * not possible and the caller should load `url` and ask again (only when
 * `reloaded` is false; after a load the failure is final). A new chat with
 * `project` opens on that project's page; when the project can be neither
 * found nor created, it is a plain new chat with a warning (the user still
 * gets an answer).
 */
export async function openTarget(
  target: ChatTarget,
  opts: OpenTargetOptions,
): Promise<OpenTargetResult> {
  const budget = (ms: number): number => Math.max(0, Math.min(ms, opts.deadline - Date.now()));
  const loadOrFail = (url: string, why: string): OpenTargetResult => {
    if (opts.reloaded) throw new TargetNotReachedError(`${why} (after a page load). ${describeDom()}`);
    return { ready: false, url };
  };

  if (target.kind === "new_chat") {
    let warning: string | undefined;
    if (opts.project !== undefined) {
      const opened = await openProjectInOwnTab(opts.project, opts);
      if (opened.ok) {
        requireComposer();
        return { ready: true, target: { kind: "new_chat", projectId: opened.projectId } };
      }
      // The project's page did not open in-page: a page load may open it.
      if (opened.url !== undefined && !opts.reloaded) return { ready: false, url: opened.url };
      warning = projectWarning(opts.project, opened.reason);
      console.warn("[cgpt-bridge]", warning);
    }
    if (!isBlankNewChat()) {
      const button = findNewChatButton();
      if (!button) return loadOrFail(`${CHATGPT_ORIGIN}/`, "New chat button not found");
      button.click();
      const opened = await waitForPage(
        () => isBlankNewChat() && findComposer() !== null,
        budget(IN_PAGE_NAV_TIMEOUT_MS),
      );
      if (!opened) return loadOrFail(`${CHATGPT_ORIGIN}/`, "New chat did not open");
      // Right after an in-page switch the new chat may still be rendering;
      // typing into it too early can lose the prompt (live 26.10.2026: "No
      // reply detected" on a new chat).
      await sleep(budget(CHAT_SETTLE_MS));
    }
    await waitForPage(() => findComposer() !== null, budget(CHAT_RENDER_TIMEOUT_MS));
    requireComposer();
    return { ready: true, target, ...(warning === undefined ? {} : { warning }) };
  }

  const path = conversationPathOfUrl(target.url);
  if (path === null) {
    throw new BadTargetError(`Not a saved chatgpt.com conversation URL: ${target.url}`);
  }
  if (targetMatchesPath(target, location.pathname)) {
    // A saved chat always has an answer; until it shows, the answer baseline
    // would be wrong, so never go on with a half-rendered chat.
    if (await waitForChatRendered(target, null, budget(CHAT_RENDER_TIMEOUT_MS))) {
      requireComposer();
      return { ready: true, target };
    }
    return loadOrFail(CHATGPT_ORIGIN + path, "Chat history did not render");
  }
  const before = getLastAssistantMessageId();
  const link = Array.from(
    document.querySelectorAll<HTMLAnchorElement>(SIDEBAR_CHAT_LINK_SELECTOR),
  ).find((a) => a.getAttribute("href") === path);
  if (link) {
    link.click();
    if (await waitForChatRendered(target, before, budget(IN_PAGE_NAV_TIMEOUT_MS))) {
      requireComposer();
      return { ready: true, target };
    }
  }
  return loadOrFail(CHATGPT_ORIGIN + path, "Could not open the chat");
}

function isBlankNewChat(): boolean {
  return location.pathname === "/" && assistantNodes().length === 0;
}

function findNewChatButton(): HTMLElement | null {
  for (const btn of Array.from(document.querySelectorAll<HTMLButtonElement>(NEW_CHAT_CYCLE_SELECTOR))) {
    if (btn.hasAttribute("data-sidebar-destination") || btn.hasAttribute("aria-haspopup")) continue;
    return btn;
  }
  // Collapsed sidebar: localized label only (en/ru).
  for (const btn of Array.from(document.querySelectorAll<HTMLButtonElement>("button[aria-label]"))) {
    if (NEW_CHAT_LABEL_RE.test((btn.getAttribute("aria-label") ?? "").trim())) return btn;
  }
  return null;
}

// True once `target` is open with its turns rendered and stable. With
// `previousAnswerId` the newest answer must differ from it: right after an
// in-page switch the old chat's turns are still in the DOM.
async function waitForChatRendered(
  target: PageTarget,
  previousAnswerId: string | null,
  timeoutMs: number,
): Promise<boolean> {
  const rendered = (): boolean =>
    targetMatchesPath(target, location.pathname) &&
    findComposer() !== null &&
    assistantNodes().length > 0 &&
    (previousAnswerId === null || getLastAssistantMessageId() !== previousAnswerId);
  const deadline = Date.now() + timeoutMs;
  while (Date.now() < deadline) {
    if (!(await waitForPage(rendered, deadline - Date.now()))) return false;
    const count = assistantNodes().length;
    await sleep(CHAT_SETTLE_MS);
    if (rendered() && assistantNodes().length === count) return true;
  }
  return false;
}

function requireComposer(): void {
  if (findComposer() === null) throw new ComposerNotFoundError(describeDom());
}

// ─── ChatGPT projects ─────────────────────────────────────────────────────
// A new chat of the bridge goes into the project named after the working
// folder (see the inventory at the top for the markup). Only attributes are
// used: every label here is localized.

const PROJECT_ROW_SELECTOR = "[data-app-action-sidebar-project-row]";
const PROJECT_LABEL_ATTR = "data-app-action-sidebar-project-label";
const PROJECT_ID_ATTR = "data-app-action-sidebar-project-id";
const PROJECT_CREATE_SELECTOR = "button[data-app-action-sidebar-project-create]";
const DIALOG_SELECTOR = "[role=\"dialog\"]";
const PROJECT_NAME_INPUT_SELECTOR = "input#chatgpt-project-name";
const DIALOG_SUBMIT_SELECTOR = "button[type=\"submit\"]";
// The whole project step gives up after this (then: a plain new chat).
const PROJECT_OPEN_TIMEOUT_MS = 30_000;
// A freshly loaded page may render the sidebar a moment after the composer.
const SIDEBAR_WAIT_MS = 5_000;
// The project rows are account data and may render after the (static)
// create button: before a project is created its row gets this long to
// show, and the list must then hold still for ROW_SETTLE_MS.
const ROW_WAIT_MS = 5_000;
const ROW_SETTLE_MS = 1_000;
// How long the create dialog gets to open.
const PROJECT_UI_WAIT_MS = 3_000;
// chrome.storage.local: { [name, NFC]: "g-p-<hex>" } of the projects found or
// created, so a project the sidebar does not show (collapsed, hidden, behind
// "more", a label the page changed) is not created again.
const PROJECT_CACHE_KEY = "chatgptProjectIds";

export type ProjectOpened =
  | { ok: true; projectId: string }
  // `url`: the project's page, which a page load may open where an in-page
  // switch did not.
  | { ok: false; reason: string; url?: string };

// A name as compared with a row's label: NFC (a macOS folder name is often
// NFD) with runs of whitespace as one space.
function nameKey(name: string): string {
  return name.normalize("NFC").replace(/\s+/g, " ").trim();
}

async function readProjectCache(): Promise<Record<string, unknown>> {
  try {
    const items = await chrome.storage.local.get(PROJECT_CACHE_KEY);
    const value: unknown = items[PROJECT_CACHE_KEY];
    return typeof value === "object" && value !== null ? { ...(value as Record<string, unknown>) } : {};
  } catch {
    return {};
  }
}

/** The id remembered for project `name`, or null. */
export async function cachedProjectId(name: string): Promise<string | null> {
  const id = (await readProjectCache())[name.normalize("NFC")];
  return typeof id === "string" && PROJECT_ID_RE.test(id) ? id.toLowerCase() : null;
}

async function rememberProject(name: string, projectId: string): Promise<void> {
  const cache = await readProjectCache();
  const key = name.normalize("NFC");
  if (cache[key] === projectId) return;
  cache[key] = projectId;
  try {
    await chrome.storage.local.set({ [PROJECT_CACHE_KEY]: cache });
  } catch (err) {
    console.warn("[cgpt-bridge] could not remember the project id:", err);
  }
}

/** Forget `projectId` for `name` (stale: another label, no page, no chat address). */
export async function forgetProject(name: string, projectId: string): Promise<void> {
  const cache = await readProjectCache();
  const key = name.normalize("NFC");
  if (cache[key] !== projectId) return;
  delete cache[key];
  try {
    await chrome.storage.local.set({ [PROJECT_CACHE_KEY]: cache });
  } catch (err) {
    console.warn("[cgpt-bridge] could not forget the project id:", err);
  }
}

function projectRows(): HTMLElement[] {
  return Array.from(document.querySelectorAll<HTMLElement>(PROJECT_ROW_SELECTOR));
}

function rowId(row: HTMLElement): string | null {
  const id = row.getAttribute(PROJECT_ID_ATTR) ?? "";
  return PROJECT_ID_RE.test(id) ? id.toLowerCase() : null;
}

// Live check 2026-10-28: with more than five projects the sidebar lists the
// five newest and a "show more" button (localized label, no data attribute)
// in its own item of the projects list; it lists the rest in place and is
// gone afterwards. Only the list's own items are looked at: an expanded
// project nests its chats, each with its own buttons ("chat actions", "pin").
function showMoreProjectsButton(): HTMLElement | null {
  const list = projectRows()[0]?.closest<HTMLElement>("[role=\"list\"]") ?? null;
  if (list === null) return null;
  for (const item of Array.from(list.children) as HTMLElement[]) {
    if (item.getAttribute("role") !== "listitem") continue;
    if (item.querySelector(PROJECT_ROW_SELECTOR) !== null) continue;
    for (const button of item.querySelectorAll<HTMLElement>("button")) {
      if (button.matches(PROJECT_CREATE_SELECTOR)) continue;
      return button;
    }
  }
  return null;
}

// The row named `name` (see nameKey), with a project id.
function findProjectRow(name: string): HTMLElement | null {
  const want = nameKey(name);
  return (
    projectRows().find(
      (row) => nameKey(row.getAttribute(PROJECT_LABEL_ATTR) ?? "") === want && rowId(row) !== null,
    ) ?? null
  );
}

function rowWithId(projectId: string): HTMLElement | null {
  return projectRows().find((row) => rowId(row) === projectId) ?? null;
}

// Whether a rendered row shows `projectId` under another name than `name`:
// a remembered id that no longer stands for that name.
function rowNamesOther(projectId: string, name: string): boolean {
  const row = rowWithId(projectId);
  return row !== null && nameKey(row.getAttribute(PROJECT_LABEL_ATTR) ?? "") !== nameKey(name);
}

// The project page of `projectId`, with no chat on it yet.
function isBlankProjectPage(projectId: string): boolean {
  return projectPageId(location.pathname) === projectId && userTurnCount() === 0;
}

// Live check 2026-10-28: clicking a project row only expands its chat list.
// The row holds three buttons in this order: expand (no menu), project
// actions (`aria-haspopup`) and "new chat in <project>" (no menu). The last
// one opens the project page, so it is the menu-less button after the
// actions button; without an actions button, the last menu-less one.
function projectRowNewChatButton(row: HTMLElement): HTMLElement | null {
  const buttons = Array.from(row.querySelectorAll<HTMLElement>("button"));
  const plain = (b: HTMLElement): boolean => b.getAttribute("aria-haspopup") === null;
  let menu = -1;
  buttons.forEach((b, i) => {
    if (!plain(b)) menu = i;
  });
  if (menu >= 0) return buttons.slice(menu + 1).find(plain) ?? null;
  return buttons.filter(plain).pop() ?? null;
}

function openDialogInput(): HTMLInputElement | null {
  for (const dialog of Array.from(document.querySelectorAll<HTMLElement>(DIALOG_SELECTOR))) {
    const input = dialog.querySelector<HTMLInputElement>(PROJECT_NAME_INPUT_SELECTOR);
    if (input !== null) return input;
  }
  return null;
}

// Close a create dialog left open by a failed attempt, as Escape does.
function closeProjectDialog(): void {
  const dialog = openDialogInput()?.closest<HTMLElement>(DIALOG_SELECTOR);
  if (!dialog) return;
  dialog.dispatchEvent(new KeyboardEvent("keydown", { key: "Escape", code: "Escape", bubbles: true }));
}

/** The warning for a new chat that went to a plain new chat instead of `name`. */
export function projectWarning(name: string, reason: string): string {
  return `ChatGPT project "${name}" was not opened or created (${reason}); the chat went to a plain new chat.`;
}

// Whether the page holds at the blank page of `projectId` with the composer
// (waited for up to `timeoutMs`, then once more after the settle time).
async function projectPageHolds(projectId: string, timeoutMs: number, settleMs: number): Promise<boolean> {
  const there = (): boolean => isBlankProjectPage(projectId) && findComposer() !== null;
  if (!(await waitForPage(there, timeoutMs))) return false;
  // As with a new chat opened in-page: typing too early can lose the prompt.
  await sleep(settleMs);
  return there();
}

/**
 * Open the page of the ChatGPT project named `name` (see nameKey), creating
 * the project when the sidebar has none by that name, before `deadline` (and
 * at most PROJECT_OPEN_TIMEOUT_MS). Resolves with the project's id once its
 * page shows, blank, with the composer, and remembers it. `cachedId`: the id
 * remembered for the name, looked for first by its row (dropped when its row
 * shows another name). Before a project is created the named row is given
 * ROW_WAIT_MS to render. Nothing is clicked to open a collapsed "Projects"
 * section (no reliable sign of one was recorded): a project it hides that
 * is not yet remembered gets a duplicate. When the row is still not in
 * view, a long list's "show more" control
 * (showMoreProjectsButton) is pressed once and the row looked for again
 * until the list stops growing (at most ROW_WAIT_MS);
 * a project hidden in some other way gets a duplicate, unless its id is
 * remembered.
 */
export async function openProject(
  name: string,
  deadline: number,
  cachedId: string | null = null,
): Promise<ProjectOpened> {
  const end = Math.min(deadline, Date.now() + PROJECT_OPEN_TIMEOUT_MS);
  const budget = (ms: number): number => Math.max(0, Math.min(ms, end - Date.now()));
  const done = async (projectId: string): Promise<ProjectOpened> => {
    await rememberProject(name, projectId);
    return { ok: true, projectId };
  };
  let cached = cachedId;
  if (cached !== null && isBlankProjectPage(cached) && findComposer() !== null) return done(cached);

  await waitForPage(
    () => projectRows().length > 0 || document.querySelector(PROJECT_CREATE_SELECTOR) !== null,
    budget(SIDEBAR_WAIT_MS),
  );
  // The remembered project's row, unless it now shows another name; else
  // the row with the name.
  let stale = false;
  const lookup = (): HTMLElement | null => {
    if (cached !== null) {
      if (!rowNamesOther(cached, name)) {
        const row = rowWithId(cached);
        if (row !== null) return row;
      } else {
        stale = true;
        cached = null;
      }
    }
    return findProjectRow(name);
  };
  let row = lookup();
  if (row === null) {
    // Before a project is created: its row may only be late.
    await waitForPage(() => lookup() !== null, budget(ROW_WAIT_MS));
    row = lookup();
    let count = projectRows().length;
    while (row === null && budget(ROW_SETTLE_MS) > 0) {
      await sleep(budget(ROW_SETTLE_MS));
      row = lookup();
      if (projectRows().length === count) break;
      count = projectRows().length;
    }
  }
  if (row === null) {
    // A long list shows only the newest projects; "show more" lists the rest
    // in place. Without it an older project would be created a second time.
    const more = showMoreProjectsButton();
    if (more !== null) {
      const shown = projectRows().length;
      more.click();
      // The rest may render in steps: after the first new rows, wait until
      // the list holds still for ROW_SETTLE_MS, ROW_WAIT_MS in all.
      const until = Date.now() + ROW_WAIT_MS;
      const left = (): number => budget(until - Date.now());
      await waitForPage(() => lookup() !== null || projectRows().length > shown, left());
      row = lookup();
      let count = projectRows().length;
      while (row === null && count > shown && left() > 0) {
        await waitForPage(
          () => lookup() !== null || projectRows().length !== count,
          Math.min(ROW_SETTLE_MS, left()),
        );
        row = lookup();
        if (projectRows().length === count) break;
        count = projectRows().length;
      }
    }
  }
  if (stale && cachedId !== null) await forgetProject(name, cachedId);

  if (row !== null) {
    const projectId = rowId(row) ?? "";
    if (isBlankProjectPage(projectId) && findComposer() !== null) return done(projectId);
    const newChat = projectRowNewChatButton(row);
    if (newChat === null) {
      return {
        ok: false,
        reason: "the project row has no new-chat button",
        url: `${CHATGPT_ORIGIN}/g/${projectId}/project`,
      };
    }
    newChat.click();
    if (await projectPageHolds(projectId, budget(IN_PAGE_NAV_TIMEOUT_MS), budget(CHAT_SETTLE_MS))) {
      return done(projectId);
    }
    return {
      ok: false,
      reason: "the project page did not open",
      url: `${CHATGPT_ORIGIN}/g/${projectId}/project`,
    };
  }

  const create = document.querySelector<HTMLElement>(PROJECT_CREATE_SELECTOR);
  if (create === null) return { ok: false, reason: "no project of that name and no create button" };
  // The new project is the one whose page opens and that was not there before.
  const known = new Set(projectRows().map(rowId));
  const here = projectPageId(location.pathname);
  create.click();
  if (!(await waitForPage(() => openDialogInput() !== null, budget(PROJECT_UI_WAIT_MS)))) {
    return { ok: false, reason: "the create dialog did not open" };
  }
  const input = openDialogInput();
  const submitButton = input?.closest<HTMLElement>(DIALOG_SELECTOR)?.querySelector<HTMLButtonElement>(DIALOG_SUBMIT_SELECTOR);
  if (!input || !submitButton) {
    closeProjectDialog();
    return { ok: false, reason: "the create dialog has no name field or submit button" };
  }
  input.focus();
  setNativeValue(input, name);
  input.dispatchEvent(new Event("input", { bubbles: true }));
  input.dispatchEvent(new Event("change", { bubbles: true }));
  if (!(await waitForPage(() => isUsableButton(submitButton), budget(PROJECT_UI_WAIT_MS)))) {
    closeProjectDialog();
    return { ok: false, reason: "the create button of the dialog stayed disabled" };
  }
  submitButton.click();
  const isNew = (): string | null => {
    const id = projectPageId(location.pathname);
    return id !== null && id !== here && !known.has(id) ? id : null;
  };
  await waitForPage(() => isNew() !== null, budget(IN_PAGE_NAV_TIMEOUT_MS));
  const projectId = isNew();
  if (projectId === null) {
    closeProjectDialog();
    return { ok: false, reason: "the new project's page did not open" };
  }
  if (await projectPageHolds(projectId, budget(IN_PAGE_NAV_TIMEOUT_MS), budget(CHAT_SETTLE_MS))) {
    return done(projectId);
  }
  return { ok: false, reason: "the new project's page did not open" };
}

/**
 * The gateway's way into project `name`, in its own tab where a page load is
 * allowed. With a remembered id it loads `/g/<id>/project` directly (the
 * sidebar may not show the project) and takes it when the page holds there
 * with the composer; a remembered id whose page does not hold, or whose row
 * shows another name, is forgotten and the sidebar is searched. After a load
 * the blank project page is taken as is when it is the one asked for (its
 * row may not have rendered yet, and creating another is worse).
 */
async function openProjectInOwnTab(name: string, opts: OpenTargetOptions): Promise<ProjectOpened> {
  const budget = (ms: number): number => Math.max(0, Math.min(ms, opts.deadline - Date.now()));
  let cached = await cachedProjectId(name);
  if (opts.reloaded) {
    const here = projectPageId(location.pathname);
    // The load was for the remembered project, or (none remembered) for the
    // row's.
    if (here !== null && here === (cached ?? here)) {
      if (await projectPageHolds(here, budget(CHAT_RENDER_TIMEOUT_MS), budget(CHAT_SETTLE_MS))) {
        await rememberProject(name, here);
        return { ok: true, projectId: here };
      }
    }
    if (cached !== null) {
      console.warn(`[cgpt-bridge] the remembered page of project "${name}" did not hold: forgotten`);
      await forgetProject(name, cached);
    }
    return openProject(name, opts.deadline);
  }
  if (cached !== null && rowNamesOther(cached, name)) {
    await forgetProject(name, cached);
    cached = null;
  }
  if (cached !== null) {
    if (isBlankProjectPage(cached) && findComposer() !== null) return { ok: true, projectId: cached };
    return { ok: false, reason: "the remembered project's page is loaded", url: `${CHATGPT_ORIGIN}/g/${cached}/project` };
  }
  return openProject(name, opts.deadline);
}

/**
 * For an ask into the user's own tab (no target): when the page shows a new
 * chat — ChatGPT's or a project's page, with no turn yet — open project
 * `name` there (the tab is never loaded: a remembered id only finds the row).
 * Null when the page shows a chat (the ask continues it) or the project
 * opened; otherwise the warning, with the page left where it is (which may
 * be another project's page).
 */
export async function openProjectForNewChat(name: string, deadline: number): Promise<string | null> {
  const path = location.pathname;
  if ((path !== "/" && projectPageId(path) === null) || userTurnCount() > 0) return null;
  const opened = await openProject(name, deadline, await cachedProjectId(name));
  if (opened.ok) return null;
  closeProjectDialog();
  const warning = `The new chat was not put into ChatGPT project "${name}" (${opened.reason}).`;
  console.warn("[cgpt-bridge]", warning);
  return warning;
}

/**
 * Press Stop on the answer being generated, waiting at most `timeoutMs` in
 * total. Resolves true when Stop was pressed, false when nothing was
 * generating. Right after submit generation may not show yet, so it is
 * given `startGraceMs` to appear (0: only if it is generating now).
 * The caller must put the pageShim to sleep first: while active it swallows
 * the page's stop request. `beforePress` runs right before the click and
 * may throw to press nothing (the page no longer shows the turn's chat).
 */
export async function stopGenerating(
  timeoutMs: number,
  startGraceMs = STOP_START_GRACE_MS,
  beforePress: () => void = () => {},
): Promise<boolean> {
  const deadline = Date.now() + timeoutMs;
  const started = await waitForPage(() => isGenerating(), Math.min(startGraceMs, timeoutMs));
  if (!started) return false;
  // Give an answer that may be just ending the rest of the budget.
  await waitOutEndingAnswer(Math.max(0, deadline - Date.now()));
  if (!isGenerating()) return false;
  beforePress();
  const button = findStopButton();
  if (!button) throw new StopButtonNotFoundError(`Stop button not found. ${describeDom()}`);
  button.click();
  await waitForPage(() => !isGenerating(), Math.max(0, deadline - Date.now()));
  return true;
}

/**
 * Whether the page is still generating an earlier answer after `graceMs`.
 * Presses nothing. Any sign is waited out: right after an answer is in,
 * ChatGPT keeps the turn `in_progress`, with Stop still shown, for some
 * seconds (seen live: an ask right after another was refused at once).
 */
export async function previousAnswerRunning(graceMs: number): Promise<boolean> {
  if (!isGenerating()) return false;
  await waitForPage(() => !isGenerating(), graceMs);
  return isGenerating();
}

// Only the turn state says "generating" and there is nothing to press: the
// answer may be just ending. Waits up to `timeoutMs` for it to end or for a
// Stop button to show.
async function waitOutEndingAnswer(timeoutMs: number): Promise<void> {
  if (findStopButton() !== null || generatingSignal() !== "turn-in_progress") return;
  await waitForPage(() => !isGenerating() || findStopButton() !== null, timeoutMs);
}

/**
 * Before a new prompt: if the open chat is still generating an earlier
 * answer (e.g. one whose ask timed out), stop it and wait for the page to
 * settle. Otherwise the send button is a Stop button and the turns would mix.
 * `beforePress` as for `stopGenerating`.
 */
export async function settlePreviousAnswer(
  timeoutMs: number,
  beforePress: () => void = () => {},
): Promise<void> {
  if (!isGenerating()) return;
  await stopGenerating(timeoutMs, 0, beforePress);
  if (isGenerating()) {
    throw new PreviousAnswerRunningError(
      `The previous answer is still generating after Stop. ${describeDom()}`,
    );
  }
}

function findStopButton(): HTMLButtonElement | null {
  return formStopButton() ?? selectorStopButton();
}

// octane: the submit button itself turns into Stop (its label changes).
function formStopButton(): HTMLButtonElement | null {
  const formBtn = composerFormSubmit();
  if (!formBtn || !isUsableButton(formBtn)) return null;
  const label = formBtn.getAttribute("aria-label");
  const changed = submittedLabel !== null && label !== null && label !== submittedLabel;
  return looksLikeStop(formBtn) || changed ? formBtn : null;
}

// Every match is looked at: a hidden Stop-like button may come first.
function selectorStopButton(): HTMLButtonElement | null {
  for (const selector of STOP_BUTTON_SELECTORS) {
    for (const btn of Array.from(document.querySelectorAll<HTMLButtonElement>(selector))) {
      if (isUsableButton(btn)) return btn;
    }
  }
  return null;
}

// Resolves true as soon as `predicate` holds, false after `timeoutMs`. Wakes
// on DOM mutations as well as on a timer: timers in a hidden tab are
// throttled, mutation callbacks are not (see waitForNewAnswer).
function waitForPage(predicate: () => boolean, timeoutMs: number): Promise<boolean> {
  return new Promise((resolve) => {
    let settled = false;
    const finish = (value: boolean): void => {
      if (settled) return;
      settled = true;
      observer.disconnect();
      clearInterval(poll);
      clearTimeout(deadline);
      resolve(value);
    };
    const check = (): void => {
      if (!settled && predicate()) finish(true);
    };
    const observer = new MutationObserver(check);
    const poll = setInterval(check, 200);
    const deadline = setTimeout(() => finish(predicate()), Math.max(0, timeoutMs));
    observer.observe(document.documentElement, { childList: true, subtree: true, attributes: true });
    check();
  });
}

// ---------------------------------------------------------------------------
// diagnose() — snapshot of what the adapter can see right now. Used by the
// `diagnose` bridge request from the SW console and (later) by `cgpt doctor`.
// Each probe returns `null`/`false` when missing so callers can pinpoint
// which selector to repair.
// ---------------------------------------------------------------------------

// One-line summary for error messages, so a markup change shows up in the
// CLI as "what was not found" instead of a bare timeout.
export function describeDom(): string {
  return summarizeDiagnostics(diagnose());
}

/** The one-line form of a `diagnose()` snapshot (also used by the background). */
export function summarizeDiagnostics(d: AdapterDiagnostics): string {
  return (
    `ui=${d.uiProfile} composer=${d.composerFound} composerText=${d.composerTextLen} ` +
    `send=${d.sendButtonFound} generating=${d.isGenerating} ` +
    `signal=${d.generatingSignal ?? "none"} answers=${d.assistantTurnCount} path=${d.pathname}`
  );
}

export interface AdapterDiagnostics {
  href: string;
  host: string;
  pathname: string;
  uiProfile: UiProfile;
  supportedPage: boolean;
  composerFound: boolean;
  composerTag: string | null;
  composerIsContentEditable: boolean | null;
  // Text still in the composer: after a submit, the prompt was not taken.
  composerTextLen: number;
  sendButtonFound: boolean;
  sendButtonDisabled: boolean | null;
  stopButtonFound: boolean;
  streamActiveAttrFound: boolean;
  isGenerating: boolean;
  generatingSignal: string | null;
  assistantTurnCount: number;
  lastAssistantMessageId: string | null;
  lastAssistantTextLen: number;
  markdownContainerFound: boolean;
  userAgent: string;
  capturedAt: number;
}

export function diagnose(): AdapterDiagnostics {
  const composer = findComposer();
  const sendBtn = findSendButton() ?? composerFormSubmit();
  const stopBtn = findStopButton();
  const signal = generatingSignal();
  const streamActive = document.querySelector(
    "[data-scroll-root][data-stream-active]",
  );
  const answers = assistantNodes();
  const lastNode = lastAssistantNode();
  const lastText = lastNode ? readAssistantText(lastNode) : null;
  const markdown = lastNode ? markdownRootOf(lastNode) : null;

  return {
    href: location.href,
    host: location.host,
    pathname: location.pathname,
    uiProfile: detectProfile(),
    supportedPage: isSupportedPage(),
    composerFound: composer !== null,
    composerTag: composer ? composer.tagName.toLowerCase() : null,
    composerIsContentEditable: composer ? composer.isContentEditable : null,
    composerTextLen: composerText(composer).length,
    sendButtonFound: sendBtn !== null,
    sendButtonDisabled: sendBtn ? sendBtn.disabled : null,
    stopButtonFound: stopBtn !== null,
    streamActiveAttrFound: streamActive !== null,
    isGenerating: signal !== null,
    generatingSignal: signal,
    assistantTurnCount: answers.length,
    lastAssistantMessageId: lastNode ? messageIdOf(lastNode) : null,
    lastAssistantTextLen: lastText?.length ?? 0,
    markdownContainerFound: markdown !== null,
    userAgent: navigator.userAgent,
    capturedAt: Date.now(),
  };
}

function composerText(composer: HTMLElement | null): string {
  if (composer === null) return "";
  if (composer instanceof HTMLTextAreaElement) return composer.value.trim();
  return (composer.textContent ?? "").trim();
}
