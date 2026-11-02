// A fake chatgpt.com window for running the real page code in Node: the
// MAIN-world pageShim.ts, the isolated-world content.ts and the adapter
// functions between them (recipe: research/CODE_AUDIT.md, appendix A,
// "Страница в Node"). No network, no browser, no new dependencies.
//
//   - `window.postMessage` delivers `{ source: window, origin, data }` to the
//     `message` listeners asynchronously, like the browser's task;
//   - `location` is a chatgpt.com address the test moves with `goTo`;
//   - `fetch` is whatever the test routes (`routeFetch`), typically a
//     `text/event-stream` Response over a ReadableStream (chatPage.ts);
//   - the DOM is a tree of FakeElements, each answering to the selector
//     strings it was created with: `querySelectorAll("a, b")` returns the
//     elements registered under "a" or "b" in document order. Only bare tag
//     names (`form`, `button`, `code`) are matched structurally;
//   - `document.execCommand` edits the text of the focused element;
//   - `chrome.runtime.onMessage` keeps the content script's listener, which
//     `sendToContent` calls the way the background would;
//   - `chrome.storage.local` is the `localStore` object (cleared by the test);
//   - `sessionStorage` is the `sessionStore` map (the test may edit it, or
//     make it throw with `sessionStorageThrows`).
//
// One window per test file. `loadPage()` installs these globals on
// globalThis and never restores them; pageShim and content.ts install
// themselves into the window once. That is safe because `npm test`
// (package.json) bundles every `test/*.test.ts` into its own self-contained
// .mjs (esbuild, one entry point each, no code splitting) and `node --test`
// runs each file in its own process: nothing leaks into another file. It
// would not be safe with `--test-isolation=none`, or in a test file that is
// not about the page and imports this harness.
//
// Tests in one file share the window and call `resetPage()` between them,
// but state inside the page code survives: pageShim's switch, and the
// adapter's label of the last Send it clicked (`submittedLabel`). So keep
// one markup family per file (the octane markup gets its own file: after
// it, the account Send button reads as a relabelled Stop), and leave no
// ask running at the end of a test (chatPage.ts `settlePage`).

type Listener = (ev: unknown) => void;

class Listeners {
  private byType = new Map<string, Listener[]>();

  add(type: string, fn: Listener): void {
    const list = this.byType.get(type) ?? [];
    list.push(fn);
    this.byType.set(type, list);
  }

  remove(type: string, fn: Listener): void {
    const list = this.byType.get(type);
    if (!list) return;
    const i = list.indexOf(fn);
    if (i >= 0) list.splice(i, 1);
  }

  dispatch(type: string, ev: unknown): void {
    for (const fn of [...(this.byType.get(type) ?? [])]) fn(ev);
  }

  count(type: string): number {
    return this.byType.get(type)?.length ?? 0;
  }
}

// ─── DOM ──────────────────────────────────────────────────────────────────

// Mutation observers are woken (asynchronously, once per burst) by any DOM
// change; they do not filter by target or mutation type.
const observers = new Set<FakeMutationObserver>();
let mutationPending = false;

function domChanged(): void {
  if (mutationPending || observers.size === 0) return;
  mutationPending = true;
  queueMicrotask(() => {
    mutationPending = false;
    for (const o of [...observers]) o.notify();
  });
}

class FakeMutationObserver {
  constructor(private callback: () => void) {}

  observe(): void {
    observers.add(this);
  }

  disconnect(): void {
    observers.delete(this);
  }

  notify(): void {
    if (observers.has(this)) this.callback();
  }
}


const TEXT_NODE = 3;
const ELEMENT_NODE = 1;

export class FakeText {
  readonly nodeType = TEXT_NODE;
  parentNode: FakeElement | null = null;
  constructor(public data: string) {}

  get parentElement(): FakeElement | null {
    return this.parentNode;
  }

  get textContent(): string {
    return this.data;
  }

  set textContent(value: string) {
    this.data = value;
    domChanged();
  }
}

export type FakeNode = FakeElement | FakeText;

export interface ElementInit {
  // Selector strings this element is found by, exactly as the adapter
  // spells them (see chatPage.ts for the ChatGPT markups).
  sels?: readonly string[];
  attrs?: Record<string, string>;
  text?: string;
  children?: readonly FakeNode[];
}

const kebab = (key: string): string => key.replace(/[A-Z]/g, (c) => "-" + c.toLowerCase());

export class FakeElement {
  readonly nodeType = ELEMENT_NODE;
  readonly tagName: string;
  readonly sels: Set<string>;
  parentNode: FakeElement | null = null;
  childNodes: FakeNode[] = [];
  disabled = false;
  hidden = false;
  // Laid out (offsetParent / bounding box); false reads as display:none.
  visible = true;
  clicks = 0;
  onClick: (() => void) | null = null;
  readonly dataset: Record<string, string | undefined>;
  private attrs = new Map<string, string>();
  private listeners = new Listeners();

  constructor(tag: string, init: ElementInit = {}) {
    this.tagName = tag.toUpperCase();
    this.sels = new Set(init.sels ?? []);
    for (const [k, v] of Object.entries(init.attrs ?? {})) this.attrs.set(k, v);
    if (init.text !== undefined) this.append(new FakeText(init.text));
    if (init.children) this.append(...init.children);
    // `dataset` reflects the data-* attributes, as in the browser.
    this.dataset = new Proxy({} as Record<string, string | undefined>, {
      get: (_t, key) => (typeof key === "string" ? this.getAttribute("data-" + kebab(key)) ?? undefined : undefined),
      set: (_t, key, value) => {
        if (typeof key === "string") this.setAttribute("data-" + kebab(key), String(value));
        return true;
      },
    });
  }

  get parentElement(): FakeElement | null {
    return this.parentNode;
  }

  get children(): FakeElement[] {
    return this.childNodes.filter((n): n is FakeElement => n instanceof FakeElement);
  }

  get id(): string {
    return this.getAttribute("id") ?? "";
  }

  get classList(): { contains(c: string): boolean } {
    const classes = (this.getAttribute("class") ?? "").split(/\s+/).filter(Boolean);
    return { contains: (c: string) => classes.includes(c) };
  }

  get isContentEditable(): boolean {
    return this.getAttribute("contenteditable") === "true";
  }

  get textContent(): string {
    return this.childNodes.map((n) => n.textContent).join("");
  }

  set textContent(value: string) {
    this.replaceChildren(...(value === "" ? [] : [new FakeText(value)]));
  }

  get offsetParent(): object | null {
    return this.visible && this.isConnected ? {} : null;
  }

  getBoundingClientRect(): { width: number; height: number } {
    return this.visible && this.isConnected ? { width: 100, height: 20 } : { width: 0, height: 0 };
  }

  get isConnected(): boolean {
    let cur: FakeElement | null = this;
    while (cur.parentNode) cur = cur.parentNode;
    return cur === fakeDocument.documentElement;
  }

  getAttribute(name: string): string | null {
    return this.attrs.get(name) ?? null;
  }

  hasAttribute(name: string): boolean {
    return this.attrs.has(name);
  }

  setAttribute(name: string, value: string): void {
    this.attrs.set(name, value);
    domChanged();
  }

  removeAttribute(name: string): void {
    this.attrs.delete(name);
    domChanged();
  }

  matches(selector: string): boolean {
    return selector.split(",").some((part) => {
      const s = part.trim();
      if (this.sels.has(s)) return true;
      return /^[a-z][a-z0-9]*$/.test(s) && s.toUpperCase() === this.tagName;
    });
  }

  closest(selector: string): FakeElement | null {
    for (let cur: FakeElement | null = this; cur; cur = cur.parentNode) {
      if (cur.matches(selector)) return cur;
    }
    return null;
  }

  querySelectorAll(selector: string): FakeElement[] {
    const out: FakeElement[] = [];
    const walk = (el: FakeElement): void => {
      for (const child of el.children) {
        if (child.matches(selector)) out.push(child);
        walk(child);
      }
    };
    walk(this);
    return out;
  }

  querySelector(selector: string): FakeElement | null {
    return this.querySelectorAll(selector)[0] ?? null;
  }

  /**
   * Where `other` sits relative to this element, as the DOM reports it:
   * FOLLOWING (4) or PRECEDING (2) in document order, with CONTAINED_BY (16)
   * or CONTAINS (8) for a descendant or ancestor; DISCONNECTED (1) when they
   * share no root.
   */
  compareDocumentPosition(other: FakeNode): number {
    if (other === this) return 0;
    const chain = (n: FakeNode): FakeNode[] => {
      const out: FakeNode[] = [n];
      for (let cur = n.parentNode; cur; cur = cur.parentNode) out.unshift(cur);
      return out;
    };
    const a = chain(this);
    const b = chain(other);
    if (a[0] !== b[0]) return 1;
    if (this.contains(other)) return 4 | 16;
    if (other instanceof FakeElement && other.contains(this)) return 2 | 8;
    let i = 0;
    while (a[i] === b[i]) i++;
    const parent = a[i - 1] as FakeElement;
    const order = parent.childNodes.indexOf(a[i] as FakeNode) < parent.childNodes.indexOf(b[i] as FakeNode);
    return order ? 4 : 2;
  }

  contains(node: FakeNode | null): boolean {
    for (let cur = node?.parentNode ?? null; cur; cur = cur.parentNode) {
      if (cur === this) return true;
    }
    return node === this;
  }

  append(...nodes: FakeNode[]): void {
    for (const node of nodes) {
      node.parentNode?.detach(node);
      node.parentNode = this;
      this.childNodes.push(node);
    }
    domChanged();
  }

  appendChild<T extends FakeNode>(node: T): T {
    this.append(node);
    return node;
  }

  replaceChildren(...nodes: FakeNode[]): void {
    for (const old of this.childNodes) old.parentNode = null;
    this.childNodes = [];
    this.append(...nodes);
  }

  remove(): void {
    this.parentNode?.detach(this);
    domChanged();
  }

  private detach(node: FakeNode): void {
    const i = this.childNodes.indexOf(node);
    if (i >= 0) this.childNodes.splice(i, 1);
    node.parentNode = null;
  }

  focus(): void {
    fakeDocument.activeElement = this;
  }

  click(): void {
    this.clicks++;
    this.dispatchEvent(new Event("click", { bubbles: true, cancelable: true }));
    this.onClick?.();
  }

  addEventListener(type: string, fn: Listener): void {
    this.listeners.add(type, fn);
  }

  removeEventListener(type: string, fn: Listener): void {
    this.listeners.remove(type, fn);
  }

  /** Runs this element's own listeners; false when one called preventDefault. */
  dispatchEvent(ev: Event): boolean {
    this.listeners.dispatch(ev.type, ev);
    return !ev.defaultPrevented;
  }
}

/** `<textarea>`: the octane composer. `value` is what the native setter writes. */
export class FakeTextArea extends FakeElement {
  private current = "";

  constructor(init: ElementInit = {}) {
    super("textarea", init);
  }

  get value(): string {
    return this.current;
  }

  set value(v: string) {
    this.current = v;
    domChanged();
  }
}

export function h(tag: string, init: ElementInit = {}): FakeElement {
  return new FakeElement(tag, init);
}

class FakeDocument {
  readonly documentElement = h("html");
  readonly body = h("body");
  activeElement: FakeElement | null = null;
  // The tab's real visibility (pageShim spoofs it while active).
  realHidden = false;
  private listeners = new Listeners();

  constructor() {
    this.documentElement.append(this.body);
  }

  get hidden(): boolean {
    return this.realHidden;
  }

  get visibilityState(): string {
    return this.realHidden ? "hidden" : "visible";
  }

  hasFocus(): boolean {
    return !this.realHidden;
  }

  querySelectorAll(selector: string): FakeElement[] {
    return this.documentElement.querySelectorAll(selector);
  }

  querySelector(selector: string): FakeElement | null {
    return this.documentElement.querySelector(selector);
  }

  createElement(tag: string): FakeElement {
    return h(tag);
  }

  createTextNode(text: string): FakeText {
    return new FakeText(text);
  }

  createRange(): { selectNodeContents(node: FakeNode): void } {
    return { selectNodeContents: () => {} };
  }

  // The commands setComposerText drives a contenteditable composer with.
  execCommand(command: string, _showUi?: boolean, value?: string): boolean {
    const el = this.activeElement;
    if (!el) return false;
    if (command === "delete") el.textContent = "";
    else if (command === "insertText") el.textContent = el.textContent + (value ?? "");
    else if (command === "insertParagraph") el.textContent = el.textContent + "\n";
    else return false;
    return true;
  }

  addEventListener(type: string, fn: Listener): void {
    this.listeners.add(type, fn);
  }

  removeEventListener(type: string, fn: Listener): void {
    this.listeners.remove(type, fn);
  }

  dispatchEvent(ev: Event): boolean {
    this.listeners.dispatch(ev.type, ev);
    return !ev.defaultPrevented;
  }
}

const fakeDocument = new FakeDocument();
export const doc = fakeDocument;

// ─── window, location, fetch ──────────────────────────────────────────────

export const ORIGIN = "https://chatgpt.com";

const fakeLocation = {
  protocol: "https:",
  host: "chatgpt.com",
  hostname: "chatgpt.com",
  origin: ORIGIN,
  pathname: "/",
  href: ORIGIN + "/",
};

/** Move the page to `pathname` (an SPA navigation: nothing reloads). */
export function goTo(pathname: string): void {
  fakeLocation.pathname = pathname;
  fakeLocation.href = ORIGIN + pathname;
  domChanged();
}

export type FetchRoute = (url: string, init?: RequestInit) => Response | Promise<Response> | null;
let routes: FetchRoute[] = [];

/** Answer `fetch` calls with `route` (the first route returning a Response wins). */
export function routeFetch(route: FetchRoute): void {
  routes.push(route);
}

const windowListeners = new Listeners();

export const win = {
  location: fakeLocation,
  // Every postMessage payload, in order (the shim's reports and acks, the
  // adapter's control messages).
  posted: [] as unknown[],
  addEventListener(type: string, fn: Listener): void {
    windowListeners.add(type, fn);
  },
  removeEventListener(type: string, fn: Listener): void {
    windowListeners.remove(type, fn);
  },
  dispatchEvent(ev: Event): boolean {
    windowListeners.dispatch(ev.type, ev);
    return !ev.defaultPrevented;
  },
  postMessage(data: unknown, targetOrigin: string): void {
    if (targetOrigin !== "*" && targetOrigin !== fakeLocation.origin) return;
    win.posted.push(data);
    setTimeout(() => windowListeners.dispatch("message", { source: win, origin: fakeLocation.origin, data }), 0);
  },
  // Replaced by pageShim's wrapper on install.
  async fetch(input: RequestInfo | URL, init?: RequestInit): Promise<Response> {
    const url = typeof input === "string" ? input : input instanceof URL ? input.toString() : input.url;
    for (const route of routes) {
      const res = await route(url, init);
      if (res) return res;
    }
    return new Response("not routed in the fake window", { status: 404 });
  },
  getSelection(): { removeAllRanges(): void; addRange(): void } {
    return { removeAllRanges: () => {}, addRange: () => {} };
  },
};

/** Listeners currently registered on `window` for `type`. */
export function windowListenerCount(type: string): number {
  return windowListeners.count(type);
}

// ─── chrome.runtime ───────────────────────────────────────────────────────

type RuntimeListener = (message: unknown, sender: object, sendResponse: (r: unknown) => void) => boolean | void;
const runtimeListeners: RuntimeListener[] = [];

/** Deliver `message` to the content script as the background would; resolves with its reply. */
export function sendToContent<T = Record<string, unknown>>(message: object): Promise<T> {
  if (runtimeListeners.length === 0) throw new Error("content.ts is not loaded (call loadPage first)");
  return new Promise((resolve) => {
    for (const fn of runtimeListeners) fn(message, {}, (r) => resolve(r as T));
  });
}

/** Listeners the content script added to `chrome.runtime.onMessage`. */
export function runtimeListenerCount(): number {
  return runtimeListeners.length;
}

const EXTENSION_ID = "fakeextensionid";
const fakeRuntime: { id?: string; onMessage: { addListener: (fn: RuntimeListener) => void } } = {
  id: EXTENSION_ID,
  onMessage: { addListener: (fn: RuntimeListener) => void runtimeListeners.push(fn) },
};

/**
 * The extension was reloaded or updated: this content script is cut off
 * ("Extension context invalidated"), and `chrome.runtime.id` is gone, as in
 * Chrome. `alive = true` puts it back for the next test.
 */
export function setExtensionContextAlive(alive: boolean): void {
  if (alive) fakeRuntime.id = EXTENSION_ID;
  else delete fakeRuntime.id;
}

/** The content script's reply to a `test.ask` (ExtensionResponse, loosely typed). */
export interface AskReply {
  id: string;
  ok: boolean;
  kind?: string;
  text?: string;
  conversationUrl?: string | null;
  errorCode?: string;
  message?: string;
}

/** A `test.ask` from the background (30 s unless `extra` says otherwise). */
export function ask(id: string, text: string, extra: Record<string, unknown> = {}): Promise<AskReply> {
  return sendToContent<AskReply>({ id, type: "test.ask", text, timeoutMs: 30_000, ...extra });
}

// ─── chrome.storage.local ─────────────────────────────────────────────────

/** What the page code stored in `chrome.storage.local` (the test may edit it). */
export const localStore: Record<string, unknown> = {};

/** Empty `chrome.storage.local`. */
export function clearLocalStore(): void {
  for (const key of Object.keys(localStore)) delete localStore[key];
}

// The values go through JSON, as Chrome stores them.
const fakeStorageLocal = {
  async get(key: string): Promise<Record<string, unknown>> {
    return key in localStore ? { [key]: JSON.parse(JSON.stringify(localStore[key])) } : {};
  },
  async set(items: Record<string, unknown>): Promise<void> {
    Object.assign(localStore, JSON.parse(JSON.stringify(items)));
  },
};

// ─── sessionStorage ───────────────────────────────────────────────────────

/** The page's sessionStorage (the test may edit it). */
export const sessionStore = new Map<string, string>();
/** While true every sessionStorage call throws, as a blocked storage does. */
export const sessionStorageThrows = { on: false };

const fakeSessionStorage = {
  getItem(key: string): string | null {
    if (sessionStorageThrows.on) throw new Error("fake: sessionStorage is not available");
    return sessionStore.get(key) ?? null;
  },
  setItem(key: string, value: string): void {
    if (sessionStorageThrows.on) throw new Error("fake: sessionStorage is not available");
    sessionStore.set(key, String(value));
  },
  removeItem(key: string): void {
    if (sessionStorageThrows.on) throw new Error("fake: sessionStorage is not available");
    sessionStore.delete(key);
  },
};

// ─── install ──────────────────────────────────────────────────────────────

class FakeInputEvent extends Event {
  readonly inputType: string;
  readonly data: string | null;
  constructor(type: string, init: EventInit & { inputType?: string; data?: string } = {}) {
    super(type, init);
    this.inputType = init.inputType ?? "";
    this.data = init.data ?? null;
  }
}

class FakeKeyboardEvent extends Event {
  readonly key: string;
  readonly code: string;
  constructor(type: string, init: EventInit & { key?: string; code?: string } = {}) {
    super(type, init);
    this.key = init.key ?? "";
    this.code = init.code ?? "";
  }
}

// What the page code reaches through globals besides window/document.
class FakeXhr {
  open(): void {}
  send(): void {}
}

// The shim logs through console.info with this tag; kept here instead of
// the test output.
const SHIM_TAG = "[cgpt-bridge:shim]";
export const shimLog: string[] = [];

let installed = false;

function install(): void {
  if (installed) return;
  installed = true;
  const g = globalThis as unknown as Record<string, unknown>;
  g.window = win;
  g.document = fakeDocument;
  g.location = fakeLocation;
  g.Document = FakeDocument;
  g.HTMLTextAreaElement = FakeTextArea;
  g.MutationObserver = FakeMutationObserver;
  g.InputEvent = FakeInputEvent;
  g.KeyboardEvent = FakeKeyboardEvent;
  g.XMLHttpRequest = FakeXhr;
  g.Node = { ELEMENT_NODE, TEXT_NODE, DOCUMENT_POSITION_PRECEDING: 2, DOCUMENT_POSITION_FOLLOWING: 4 };
  g.NodeFilter = { SHOW_TEXT: 4 };
  Object.defineProperty(g, "sessionStorage", { value: fakeSessionStorage, configurable: true, writable: true });
  g.chrome = {
    runtime: fakeRuntime,
    storage: { local: fakeStorageLocal },
  };
  const info = console.info.bind(console);
  console.info = (...args: unknown[]) => {
    if (args[0] === SHIM_TAG) shimLog.push(args.slice(1).map(String).join(" "));
    else info(...args);
  };
}

/**
 * Install the fake window and load the real pageShim.ts (MAIN world) and
 * content.ts (isolated world) into it. Idempotent.
 */
export async function loadPage(): Promise<void> {
  install();
  await import("../../src/pageShim");
  await import("../../src/content");
}

/** Empty the page and move it to `pathname`; fetch routes are dropped (see the header). */
export function resetPage(pathname = "/"): void {
  fakeDocument.body.replaceChildren();
  fakeDocument.activeElement = null;
  fakeDocument.realHidden = false;
  routes = [];
  win.posted = [];
  goTo(pathname);
}

export const sleep = (ms: number): Promise<void> => new Promise((r) => setTimeout(r, ms));

/** Resolves once `predicate` holds; fails naming `what` after `timeoutMs`. */
export async function until(predicate: () => boolean, what: string, timeoutMs = 5_000): Promise<void> {
  const end = Date.now() + timeoutMs;
  while (!predicate()) {
    if (Date.now() > end) throw new Error(`timed out after ${timeoutMs} ms waiting for ${what}`);
    await sleep(10);
  }
}

/** The pageShim's latest acknowledgement: true while it is switched on. */
export function shimActive(): boolean | null {
  for (let i = win.posted.length - 1; i >= 0; i--) {
    const d = win.posted[i] as Record<string, unknown> | null;
    if (d && d.__cgptBridgeAck === true) return d.active === true;
  }
  return null;
}

/** The pageShim's reports to the content script (`sse-start`, `sse-tokens`, `sse-done`, `sse-error`). */
export function shimReports(): Array<{ kind: string; text?: string; status?: number }> {
  return win.posted.filter(
    (d): d is { kind: string } => !!d && typeof d === "object" && (d as Record<string, unknown>).__cgptBridge === true,
  );
}
