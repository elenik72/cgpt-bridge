// HTML snapshots on the fake window (fakeWindow.ts): a page layout written
// as an .html file is parsed into FakeElements, so the adapter's DOM readers
// (readAssistantText, walkNodeForText, findCodeBlockLanguage, detectProfile)
// run on it unchanged. No network, no browser, no new dependencies.
//
// This is deliberately small, not an HTML5 parser:
//   - well-formed input only (every non-void tag closed; a mismatch throws),
//     tags and attribute names are lower-cased, entities `&lt; &gt; &amp;
//     &quot; &#39; &nbsp; &#N; &#xH;` are decoded;
//   - a text node that is only whitespace with a line break in it is the
//     file's indentation and is dropped, except inside <pre> / <code> (React
//     renders no such nodes, and code keeps its own newlines);
//   - selectors are real: unlike the elements of chatPage.ts, a snapshot
//     element is not registered under selector strings, it answers
//     `matches` / `closest` / `querySelectorAll` by evaluating the selector
//     against its tag, id, classes and attributes. Supported: lists with
//     `,`, tag, `*`, `#id`, `.class`, `[attr]`, `[attr=v]`, `^= $= *= ~=`,
//     the `i` flag, `:has(<selector list>)`, descendant and `>` combinators.
//     Anything else throws, so a selector the adapter starts to use is not
//     silently read as "no match" (extend the evaluator then);
//   - the DOM calls the adapter's code walkers need on top of fakeWindow.ts:
//     an iterable `classList` and `document.createTreeWalker` (installed by
//     `loadSnapshot`).

import { FakeElement, FakeText, doc } from "./fakeWindow";
import type { FakeNode } from "./fakeWindow";

// ─── selectors ────────────────────────────────────────────────────────────

interface AttrTest {
  name: string;
  op: string | null;
  value: string;
  caseless: boolean;
}

interface Compound {
  tag: string | null;
  attrs: AttrTest[];
  has: string[][];
}

// Top-level split of `s` on `sep`, outside brackets, parentheses and quotes.
function splitTop(s: string, sep: (c: string) => boolean): string[] {
  const out: string[] = [];
  let depth = 0;
  let quote = "";
  let cur = "";
  for (const c of s) {
    if (quote) {
      if (c === quote) quote = "";
    } else if (c === '"' || c === "'") quote = c;
    else if (c === "[" || c === "(") depth++;
    else if (c === "]" || c === ")") depth--;
    else if (depth === 0 && sep(c)) {
      out.push(cur);
      cur = "";
      continue;
    }
    cur += c;
  }
  out.push(cur);
  return out;
}

const unsupported = (selector: string): Error => new Error(`snapshot selector not supported: ${selector}`);

function parseCompound(s: string, whole: string): Compound {
  const out: Compound = { tag: null, attrs: [], has: [] };
  let i = 0;
  const ident = (): string => {
    const m = /^[\w-]+/.exec(s.slice(i));
    if (!m) throw unsupported(whole);
    i += m[0].length;
    return m[0];
  };
  const balanced = (open: string, close: string): string => {
    let depth = 0;
    let quote = "";
    const start = i + 1;
    for (; i < s.length; i++) {
      const c = s[i];
      if (quote) {
        if (c === quote) quote = "";
      } else if (c === '"' || c === "'") quote = c;
      else if (c === open) depth++;
      else if (c === close && --depth === 0) return s.slice(start, i++);
    }
    throw unsupported(whole);
  };
  while (i < s.length) {
    const c = s[i];
    if (c === "*") i++;
    else if (c === "#") {
      i++;
      out.attrs.push({ name: "id", op: "=", value: ident(), caseless: false });
    } else if (c === ".") {
      i++;
      out.attrs.push({ name: "class", op: "~=", value: ident(), caseless: false });
    } else if (c === "[") {
      const body = balanced("[", "]");
      const m = /^\s*([\w-]+)\s*(?:([~^$*]?=)\s*(?:"([^"]*)"|'([^']*)'|([^\s\]]+))\s*(i)?)?\s*$/.exec(body);
      if (!m) throw unsupported(whole);
      out.attrs.push({
        name: (m[1] ?? "").toLowerCase(),
        op: m[2] ?? null,
        value: m[3] ?? m[4] ?? m[5] ?? "",
        caseless: m[6] === "i",
      });
    } else if (c === ":") {
      i++;
      if (ident() !== "has" || s[i] !== "(") throw unsupported(whole);
      out.has.push(splitTop(balanced("(", ")"), (x) => x === ",").map((p) => p.trim()));
    } else if (i === 0 && /[a-zA-Z]/.test(c ?? "")) out.tag = ident().toLowerCase();
    else throw unsupported(whole);
  }
  return out;
}

function matchesCompound(el: FakeElement, c: Compound): boolean {
  if (c.tag !== null && el.tagName.toLowerCase() !== c.tag) return false;
  for (const a of c.attrs) {
    const raw = el.getAttribute(a.name);
    if (raw === null) return false;
    if (a.op === null) continue;
    const got = a.caseless ? raw.toLowerCase() : raw;
    const want = a.caseless ? a.value.toLowerCase() : a.value;
    const ok =
      a.op === "=" ? got === want
      : a.op === "^=" ? got.startsWith(want)
      : a.op === "$=" ? got.endsWith(want)
      : a.op === "*=" ? got.includes(want)
      : got.split(/\s+/).includes(want);
    if (!ok) return false;
  }
  return c.has.every((list) => hasDescendant(el, list));
}

function hasDescendant(el: FakeElement, list: string[]): boolean {
  return el.children.some((child) => matchesList(child, list) || hasDescendant(child, list));
}

const complexTokens = (selector: string): string[] =>
  splitTop(selector.replace(/\s*>\s*/g, " > ").trim(), (c) => /\s/.test(c)).filter(Boolean);

// Selector lists already checked by `validateList`.
const validated = new Set<string>();

// Parse the whole selector, every branch of a list and every `:has` argument,
// before anything is matched: `some` would stop at the first branch that
// matches and never reach an unsupported one after it.
function validateList(list: readonly string[]): void {
  for (const s of list) {
    if (s.length === 0 || validated.has(s)) continue;
    const tokens = complexTokens(s);
    tokens.forEach((tok, i) => {
      if (tok === ">") {
        if (i === 0 || i === tokens.length - 1 || tokens[i - 1] === ">") throw unsupported(s);
        return;
      }
      for (const inner of parseCompound(tok, s).has) validateList(inner);
    });
    validated.add(s);
  }
}

// One complex selector (compounds joined by descendant or `>` combinators),
// matched from the right.
function matchesComplex(el: FakeElement, selector: string): boolean {
  const tokens = complexTokens(selector);
  const walk = (node: FakeElement, end: number): boolean => {
    const tok = tokens[end];
    if (tok === undefined || tok === ">") throw unsupported(selector);
    if (!matchesCompound(node, parseCompound(tok, selector))) return false;
    if (end === 0) return true;
    const direct = tokens[end - 1] === ">";
    const next = direct ? end - 2 : end - 1;
    if (next < 0) throw unsupported(selector);
    for (let up = node.parentNode; up; up = direct ? null : up.parentNode) {
      if (walk(up, next)) return true;
    }
    return false;
  };
  return walk(el, tokens.length - 1);
}

function matchesList(el: FakeElement, list: readonly string[]): boolean {
  validateList(list);
  return list.some((s) => s.length > 0 && matchesComplex(el, s));
}

// ─── elements ─────────────────────────────────────────────────────────────

class SnapshotElement extends FakeElement {
  constructor(tag: string, attrs: Record<string, string>) {
    super(tag, { attrs });
  }

  override get classList(): { contains(c: string): boolean } & Iterable<string> {
    const classes = (this.getAttribute("class") ?? "").split(/\s+/).filter(Boolean);
    return { contains: (c: string) => classes.includes(c), [Symbol.iterator]: () => classes[Symbol.iterator]() };
  }

  override matches(selector: string): boolean {
    return matchesList(this, splitTop(selector, (c) => c === ",").map((s) => s.trim()));
  }
}

// ─── parser ───────────────────────────────────────────────────────────────

const VOID_TAGS = new Set(["br", "hr", "img", "input", "meta", "link", "wbr"]);
const KEEP_WHITESPACE_IN = new Set(["pre", "code"]);

const TOKEN =
  /<!--[\s\S]*?-->|<!doctype[^>]*>|<\/([a-zA-Z][\w-]*)\s*>|<([a-zA-Z][\w-]*)((?:\s+[^\s"'<>/=]+(?:\s*=\s*(?:"[^"]*"|'[^']*'|[^\s"'<>]+))?)*)\s*(\/?)>|([^<]+)/gi;
const ATTRIBUTE = /([^\s"'<>/=]+)(?:\s*=\s*(?:"([^"]*)"|'([^']*)'|([^\s"'<>]+)))?/g;

const ENTITIES: Record<string, string> = { lt: "<", gt: ">", amp: "&", quot: '"', apos: "'", nbsp: " " };

function decode(s: string): string {
  return s.replace(/&(#x[0-9a-f]+|#\d+|[a-z]+);/gi, (whole, name: string) => {
    if (name.startsWith("#x") || name.startsWith("#X")) return String.fromCodePoint(parseInt(name.slice(2), 16));
    if (name.startsWith("#")) return String.fromCodePoint(parseInt(name.slice(1), 10));
    return ENTITIES[name.toLowerCase()] ?? whole;
  });
}

/** The nodes of `html` (a fragment or a whole page), detached. */
export function parseHtml(html: string): FakeNode[] {
  const roots: FakeNode[] = [];
  const stack: SnapshotElement[] = [];
  const add = (node: FakeNode): void => {
    const parent = stack[stack.length - 1];
    if (parent) parent.append(node);
    else roots.push(node);
  };
  let pos = 0;
  for (const m of html.matchAll(TOKEN)) {
    if (m.index !== pos) throw new Error(`snapshot HTML not understood near: ${html.slice(pos, pos + 40)}`);
    pos = m.index + m[0].length;
    const [whole, closing, opening, attrText, selfClose, text] = m;
    if (text !== undefined) {
      const inCode = stack.some((el) => KEEP_WHITESPACE_IN.has(el.tagName.toLowerCase()));
      if (inCode || !/^\s*$/.test(text) || !text.includes("\n")) add(new FakeText(decode(text)));
    } else if (closing !== undefined) {
      const open = stack.pop();
      if (!open || open.tagName.toLowerCase() !== closing.toLowerCase()) {
        throw new Error(`snapshot HTML: </${closing}> does not close <${open?.tagName.toLowerCase() ?? "nothing"}>`);
      }
    } else if (opening !== undefined) {
      const attrs: Record<string, string> = {};
      for (const a of (attrText ?? "").matchAll(ATTRIBUTE)) {
        attrs[(a[1] ?? "").toLowerCase()] = decode(a[2] ?? a[3] ?? a[4] ?? "");
      }
      const el = new SnapshotElement(opening.toLowerCase(), attrs);
      add(el);
      if (!VOID_TAGS.has(opening.toLowerCase()) && selfClose !== "/") stack.push(el);
    } else if (!whole.startsWith("<!")) {
      throw new Error(`snapshot HTML not understood: ${whole.slice(0, 40)}`);
    }
  }
  if (pos !== html.length) throw new Error(`snapshot HTML not understood near: ${html.slice(pos, pos + 40)}`);
  if (stack.length > 0) throw new Error(`snapshot HTML: <${stack[stack.length - 1]?.tagName.toLowerCase()}> is never closed`);
  return roots;
}

// The tree walker the adapter reads text nodes with (SHOW_TEXT only).
function createTreeWalker(root: FakeElement): { nextNode(): FakeText | null } {
  const texts: FakeText[] = [];
  const collect = (el: FakeElement): void => {
    for (const child of el.childNodes) {
      if (child instanceof FakeText) texts.push(child);
      else collect(child);
    }
  };
  collect(root);
  let i = 0;
  return { nextNode: () => texts[i++] ?? null };
}

/**
 * Put the snapshot `html` into the page body. Call after `resetPage()`;
 * `loadPage()` must have run (it installs the document globals).
 */
export function loadSnapshot(html: string): void {
  (doc as unknown as { createTreeWalker: typeof createTreeWalker }).createTreeWalker = createTreeWalker;
  doc.body.append(...parseHtml(html));
}
