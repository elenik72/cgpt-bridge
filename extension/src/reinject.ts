// Puts the page scripts into ChatGPT tabs that were open before this
// extension was installed, updated or reloaded. Chrome injects the
// manifest's content scripts only into pages loaded after that, and the
// scripts an earlier version left in a page no longer reach the extension:
// without this, a message to such a tab fails with "Receiving end does not
// exist" until the user reloads it. Needs the `scripting` permission.
//
// Both scripts run at most once per page, so injecting into a page that
// already has them changes nothing: content.ts adds its message listener
// once per isolated world (`installContentScript`), pageShim.ts wraps fetch
// once per page (`__cgptBridgePageShim`). A page that still has an earlier
// version's shim therefore keeps it (a wrapper cannot be taken back off
// fetch). content.ts uses it only when it speaks the same protocol
// (PAGE_SHIM_PROTOCOL in bridgeProtocol.ts, `setPageShimActive`); a shim of
// another version, or one so old its acknowledgement has no epoch, is not
// used, and the answer then comes from the DOM alone. A page with no shim at
// all (open since before the first install) gets it.

const CHATGPT_TABS = "https://chatgpt.com/*";
const CHATGPT_PREFIX = "https://chatgpt.com/";
// The longest tab title an error repeats (a chat title is the user's text).
const TITLE_MAX_CHARS = 60;
// An injection goes in when its page reaches document_idle; a page stuck
// loading never does, and a request must not wait for it (the host gives a
// `stop` or `status` 10 s, and the tab's next asks queue behind).
const INJECT_TIMEOUT_MS = 5_000;

/** The subset of `chrome.*` this module uses (promise forms). */
export interface InjectApi {
  tabs: {
    query(queryInfo: chrome.tabs.QueryInfo): Promise<chrome.tabs.Tab[]>;
  };
  scripting: {
    executeScript(injection: chrome.scripting.ScriptInjection<[], unknown>): Promise<unknown>;
  };
}

/**
 * Injects the page shim (the page's own world) and the content script into
 * `tabId`, requested in that order and waited for together. Null once the
 * content script went in, else Chrome's reason why not (a discarded tab, an
 * error page, a tab on another site) or the timeout.
 */
export async function injectPageScripts(api: InjectApi, tabId: number): Promise<string | null> {
  const [shim, content] = await Promise.allSettled([
    withinTimeout(
      api.scripting.executeScript({ target: { tabId }, files: ["pageShim.js"], world: "MAIN" }),
    ),
    withinTimeout(api.scripting.executeScript({ target: { tabId }, files: ["content.js"] })),
  ]);
  // A failed shim then failed for the same reason: one line is enough.
  if (content.status === "rejected") return withoutQueries(errorText(content.reason));
  if (shim.status === "rejected") {
    // The content script then answers from the DOM alone.
    console.warn(
      `[cgpt-bridge] tab ${tabId}: page shim not injected: ${withoutQueries(errorText(shim.reason))}`,
    );
  }
  return null;
}

/**
 * On install or update: the page scripts into every open ChatGPT tab. A
 * tab that cannot take them is one log line; the others still get them.
 */
export async function injectIntoOpenChatTabs(api: InjectApi): Promise<void> {
  let tabs: chrome.tabs.Tab[];
  try {
    tabs = await api.tabs.query({ url: CHATGPT_TABS });
  } catch (err) {
    console.warn(`[cgpt-bridge] could not list the ChatGPT tabs to inject into: ${errorText(err)}`);
    return;
  }
  await Promise.all(
    tabs.map(async (tab) => {
      if (typeof tab.id !== "number") return;
      const failed = await injectPageScripts(api, tab.id);
      if (failed !== null) {
        console.warn(`[cgpt-bridge] tab ${tab.id}: content script not injected: ${failed}`);
      }
    }),
  );
}

/** True for an address on chatgpt.com, the only site the scripts may go into. */
export function onChatGpt(url: string | undefined): boolean {
  return typeof url === "string" && url.startsWith(CHATGPT_PREFIX);
}

/**
 * A tab for an error message: its title, cut to TITLE_MAX_CHARS, and its
 * address without query or fragment.
 */
export function describeTab(tab: { title?: string; url?: string } | null): string {
  if (tab === null) return "a tab that is gone";
  const where = addressOf(tab.url ?? "(no address)");
  const title = tab.title ?? "";
  if (title === "") return `an untitled tab at ${where}`;
  const short = title.length > TITLE_MAX_CHARS ? `${title.slice(0, TITLE_MAX_CHARS - 1)}…` : title;
  return `the tab "${short}" at ${where}`;
}

/** `url` without its query or fragment (they may carry tokens, say a sign-in's state). */
export function addressOf(url: string): string {
  try {
    const parsed = new URL(url);
    return parsed.origin + parsed.pathname;
  } catch {
    return url.split(/[?#]/)[0] ?? "";
  }
}

// Chrome's reasons can quote a whole address ("Cannot access contents of
// url …"): every address in `text` loses its query and fragment.
function withoutQueries(text: string): string {
  return text.replace(/https?:\/\/[^\s"'<>]+/g, (url) => addressOf(url));
}

// `injection`, or an error once INJECT_TIMEOUT_MS passed. A script that goes
// in later does no harm: it runs once per page, and nothing is re-sent.
function withinTimeout(injection: Promise<unknown>): Promise<unknown> {
  return new Promise((resolve, reject) => {
    const timer = setTimeout(
      () => reject(new Error(`the page did not take the script within ${INJECT_TIMEOUT_MS} ms (still loading?)`)),
      INJECT_TIMEOUT_MS,
    );
    injection.then(
      (value) => {
        clearTimeout(timer);
        resolve(value);
      },
      (err: unknown) => {
        clearTimeout(timer);
        reject(err);
      },
    );
  });
}

function errorText(err: unknown): string {
  return err instanceof Error ? err.message : String(err);
}
