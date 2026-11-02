// How the background runs bridge asks: one at a time per tab, within the
// caller's deadline, cancellable by `stop`. Pure bookkeeping, no chrome.*,
// so it is testable on its own (test/ask.test.ts).

import type { ChatTarget } from "./bridgeProtocol";
import { conversationPathOfUrl } from "./chatgptAdapter";

// Never start typing or submit a prompt with less time than this left: the
// caller would give up before ChatGPT answers, and the turn would be left
// orphaned in the chat.
export const MIN_SUBMIT_BUDGET_MS = 15_000;

/**
 * Time that must be left to send a prompt. For short CLI timeouts (under
 * 30 s) half of the timeout, so `--timeout-ms 10000` still sends.
 */
export function submitBudgetMs(timeoutMs: number): number {
  return Math.min(MIN_SUBMIT_BUDGET_MS, timeoutMs / 2);
}

export type QueueResult<T> = { ran: true; value: T } | { ran: false };

/**
 * Runs jobs one at a time per tab, in arrival order. A job whose deadline
 * passes while it waits for its turn is dropped (`ran: false`) without being
 * started; jobs behind it still wait for the ones in front.
 */
export class TabQueue {
  private tails = new Map<number, Promise<void>>();

  async run<T>(tabId: number, deadline: number, job: () => Promise<T>): Promise<QueueResult<T>> {
    const prev = this.tails.get(tabId) ?? Promise.resolve();
    let release!: () => void;
    const mine = new Promise<void>((resolve) => {
      release = resolve;
    });
    const tail = prev.then(() => mine);
    this.tails.set(tabId, tail);
    void tail.then(() => {
      if (this.tails.get(tabId) === tail) this.tails.delete(tabId);
    });
    try {
      if (!(await settlesBefore(prev, deadline))) return { ran: false };
      return { ran: true, value: await job() };
    } finally {
      release();
    }
  }
}

function settlesBefore(p: Promise<void>, deadline: number): Promise<boolean> {
  return new Promise((resolve) => {
    const timer = setTimeout(() => resolve(false), Math.max(0, deadline - Date.now()));
    void p.then(() => {
      clearTimeout(timer);
      resolve(true);
    });
  });
}

/** A bridge ask in flight: choosing its tab, queued in it or running there. */
export interface ActiveAsk {
  // Null while the tab is still being chosen (`tabForAsk`).
  tabId: number | null;
  // The host's id of the ask; a `stop_ask` names it.
  hostId: string;
  // Runs in the extension's own tab (gateway) rather than a user tab.
  own: boolean;
  // Id of the request sent to the content script; `stop` names it.
  contentRequestId: string;
  // Set by `stop`: do not send, re-send or submit any more.
  cancelled: boolean;
  // Set right before the request goes to the content script. Until then a
  // stop has nothing to press: the ask ends `cancelled` and its prompt never
  // reaches the tab.
  handedToTab: boolean;
}

/** Which tabs have a bridge ask in flight (at most one per tab). */
export class InFlight {
  private asks: ActiveAsk[] = [];

  start(ask: Omit<ActiveAsk, "cancelled" | "handedToTab">): ActiveAsk {
    const active: ActiveAsk = { ...ask, cancelled: false, handedToTab: false };
    this.asks.push(active);
    return active;
  }

  finish(ask: ActiveAsk): void {
    this.asks = this.asks.filter((a) => a !== ask);
  }

  /**
   * The ask a `stop` (the gateway's) is about: the one in the own tab, one
   * that has its tab first. A CLI ask in a user tab is never its target.
   * Null when no own-tab ask is in flight.
   */
  forStop(): ActiveAsk | null {
    return this.asks.find((a) => a.own && a.tabId !== null) ?? this.asks.find((a) => a.own) ?? null;
  }

  /**
   * The ask a `stop_ask` names (Ctrl+C in `cgpt ask`), in whatever tab it
   * runs or waits. Null when no such ask is in flight.
   */
  named(hostId: string): ActiveAsk | null {
    return this.asks.find((a) => a.hostId === hostId) ?? null;
  }

  /** Every ask in the own tab, the one running and those queued behind it. */
  ownAsks(): ActiveAsk[] {
    return this.asks.filter((a) => a.own);
  }
}

export type ParsedTarget =
  | { ok: true; target: ChatTarget | undefined }
  | { ok: false; message: string };

/** Validate the `target` of a host `ask` frame. */
export function parseHostTarget(value: unknown): ParsedTarget {
  if (value === undefined || value === null) return { ok: true, target: undefined };
  if (typeof value !== "object") return { ok: false, message: "target must be an object" };
  const t = value as Record<string, unknown>;
  if (t.kind === "new_chat") return { ok: true, target: { kind: "new_chat" } };
  if (t.kind === "conversation") {
    if (typeof t.url === "string" && conversationPathOfUrl(t.url) !== null) {
      return { ok: true, target: { kind: "conversation", url: t.url } };
    }
    return {
      ok: false,
      message: `target is not a saved chatgpt.com conversation URL: ${String(t.url)}`,
    };
  }
  return { ok: false, message: `unknown target kind ${JSON.stringify(t.kind)}` };
}

/**
 * True when `chrome.tabs.sendMessage` failed because no content script was
 * listening yet (page still loading). Only then may a request be re-sent:
 * any other failure means the script took the request and the page went
 * away mid-turn, so re-sending could submit the prompt twice.
 */
export function isReceivingEndMissing(message: string): boolean {
  return /Receiving end does not exist|Could not establish connection/i.test(message);
}

/** The content script's view of the bridge ask running in its page. */
export interface PageAskState {
  id: string;
  cancelled: boolean;
  submitted: boolean;
  done: boolean;
}

export type StopAction = "cancel" | "press_for_ask" | "press_if_generating" | "ignore";

/**
 * What a `stop` does in a page. `askId` names the ask the background has in
 * flight; without it the background knows of none (the ask already ended,
 * e.g. by timeout, or the service worker restarted) and the stop is for the
 * own tab as a whole.
 *   cancel              — the ask has not sent its prompt: abort it, touch nothing;
 *   press_for_ask       — the ask's answer is being generated: press Stop;
 *   press_if_generating — no running ask: press Stop if the page still generates;
 *   ignore              — the named ask is over (or another one runs).
 */
export function stopActionFor(ask: PageAskState | null, askId: string | undefined): StopAction {
  const running = ask !== null && !ask.done ? ask : null;
  if (running !== null && (askId === undefined || running.id === askId)) {
    return running.submitted ? "press_for_ask" : "cancel";
  }
  return askId === undefined ? "press_if_generating" : "ignore";
}

/**
 * `stopActionFor`, applied: a stop about the running ask marks it cancelled,
 * so nothing more reaches the chat for it — before its prompt is sent it
 * aborts, after that it never clicks Send again (a click being re-tried
 * while the page confirms the first one, see `submit`).
 */
export function takeStop(ask: PageAskState | null, askId: string | undefined): StopAction {
  const action = stopActionFor(ask, askId);
  if (ask !== null && (action === "cancel" || action === "press_for_ask")) ask.cancelled = true;
  return action;
}

export interface TabLoadTimeout {
  timeoutMs: number;
  // The URL being loaded, or what else the wait was for.
  waitingFor: string;
  // The tab when the wait gave up; null when it is gone.
  tab: { status?: string; url?: string; pendingUrl?: string } | null;
  // `diagnose` summary of the page in the tab; null when it did not answer.
  page: string | null;
}

// An address without its query and fragment: a sign-in redirect carries
// tokens there, and error texts reach the CLI and the gateway log.
function withoutQuery(url: string): string {
  return url.split(/[?#]/, 1)[0] ?? "";
}

/** The error for a tab that did not finish loading, with what it showed. */
export function tabLoadTimeoutMessage(t: TabLoadTimeout): string {
  const tab =
    t.tab === null
      ? "tab gone"
      : `tab status=${t.tab.status ?? "?"} url=${t.tab.url ? withoutQuery(t.tab.url) : "?"}` +
        (t.tab.pendingUrl ? ` pendingUrl=${withoutQuery(t.tab.pendingUrl)}` : "");
  return (
    `the ChatGPT tab did not load within ${t.timeoutMs} ms ` +
    `(waiting for ${t.waitingFor}; ${tab}; page: ${t.page ?? "no answer"})`
  );
}
