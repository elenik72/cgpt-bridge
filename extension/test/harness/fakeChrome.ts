// A fake `chrome` global for loading src/bridge.ts, which reads it when it is
// imported. Import this module BEFORE `../src/bridge` (esbuild keeps import
// order). The pieces are inert until a test fills them: `useTabs` plugs a
// FakeTabs in as `chrome.tabs`, `world.content` plays the content script,
// `world.inject` plays `chrome.scripting.executeScript`, and `world.port` is
// the native port the bridge opens. Nothing here reaches a browser, the
// native host or ChatGPT.

import type { ExtensionRequest, ExtensionResponse } from "../../src/bridgeProtocol";
import type { FakeTabs } from "./fakeTabs";

/** What the fake content script answers to a message. */
export type ContentReply =
  | ExtensionResponse
  // `chrome.runtime.lastError` is set for the callback (no listener, page gone).
  | { lastError: string }
  // The callback gets no response at all.
  | undefined;

export const world = {
  // Frames the bridge posted to the native host (pings left out, see `pings`).
  posted: [] as Array<Record<string, unknown>>,
  // Messages sent to the tab, in order.
  sent: [] as Array<{ tabId: number; req: ExtensionRequest }>,
  session: {} as Record<string, unknown>,
  // `chrome.storage.local` (the gateway window's mark record).
  local: {} as Record<string, unknown>,
  content: (async () => ({ lastError: "no content script" })) as (
    tabId: number,
    req: ExtensionRequest,
  ) => Promise<ContentReply>,
  hostMessage: null as null | ((msg: unknown) => void),
  connects: 0,
  // `connectNative` throws while this is set (a host that cannot start).
  connectThrows: false,
  // When each `connectNative` call happened (Date.now, mocked in the tests).
  connectTimes: [] as number[],
  // Pings the bridge sent to the native host (kept apart from `posted`).
  pings: [] as Array<Record<string, unknown>>,
  // The host goes away: fires the disconnect listeners of the open port.
  disconnect: (): void => {},
  // Scripts injected with `chrome.scripting.executeScript`, in order.
  injected: [] as Injection[],
  // What an injection does besides being logged; a rejection is Chrome's
  // error (discarded tab, error page).
  inject: (async () => {}) as (injection: Injection) => Promise<void>,
  // `chrome.runtime.onInstalled` listeners (background.ts).
  onInstalled: [] as Array<(details: { reason: string }) => void>,
  // `chrome.runtime.onStartup` listeners (background.ts).
  onStartup: [] as Array<() => void>,
};

/** One `chrome.scripting.executeScript` call: the tab, the file, the world. */
export interface Injection {
  tabId: number;
  file: string;
  world: string;
}

const chromeStub = {
  runtime: {
    lastError: undefined as { message: string } | undefined,
    connectNative() {
      world.connects += 1;
      world.connectTimes.push(Date.now());
      if (world.connectThrows) throw new Error("fakeChrome: native host cannot start");
      const onDisconnect: Array<() => void> = [];
      world.disconnect = () => onDisconnect.forEach((cb) => cb());
      return {
        postMessage(frame: Record<string, unknown>) {
          if (frame.type === "ping") world.pings.push(frame);
          else world.posted.push(frame);
        },
        onMessage: { addListener: (cb: (msg: unknown) => void) => void (world.hostMessage = cb) },
        onDisconnect: { addListener: (cb: () => void) => void onDisconnect.push(cb) },
      };
    },
    onStartup: { addListener: (cb: () => void) => void world.onStartup.push(cb) },
    onInstalled: {
      addListener: (cb: (details: { reason: string }) => void) => void world.onInstalled.push(cb),
    },
  },
  action: { onClicked: { addListener: () => {} } },
  scripting: {
    async executeScript(injection: {
      target: { tabId: number };
      files: string[];
      world?: string;
    }): Promise<unknown[]> {
      const one: Injection = {
        tabId: injection.target.tabId,
        file: injection.files.join(","),
        world: injection.world ?? "ISOLATED",
      };
      world.injected.push(one);
      await world.inject(one);
      return [];
    },
  },
  tabs: {} as Record<string, unknown>,
  storage: {
    session: {
      async get(key: string) {
        return key in world.session ? { [key]: world.session[key] } : {};
      },
      async set(items: Record<string, unknown>) {
        Object.assign(world.session, items);
      },
    },
    local: {
      async get(key: string) {
        return key in world.local ? { [key]: JSON.parse(JSON.stringify(world.local[key])) } : {};
      },
      async set(items: Record<string, unknown>) {
        Object.assign(world.local, JSON.parse(JSON.stringify(items)));
      },
      async remove(key: string) {
        delete world.local[key];
      },
    },
  },
  windows: {
    async create() {
      throw new Error("fakeChrome: no window may be opened");
    },
  },
};

(globalThis as { chrome?: unknown }).chrome = chromeStub;

/** Make `tabs` the `chrome.tabs` the bridge sees, messages included. */
export function useTabs(tabs: FakeTabs): void {
  chromeStub.tabs = {
    ...tabs.api.tabs,
    // The gateway window takes no further tab: a second gateway ask waits in
    // the queue of the one tab, as when the window is full.
    async create() {
      throw new Error("fakeChrome: no tab may be opened");
    },
    sendMessage(
      tabId: number,
      req: ExtensionRequest,
      cb: (resp: ExtensionResponse | undefined) => void,
    ): void {
      world.sent.push({ tabId, req });
      void world.content(tabId, req).then((reply) => {
        if (reply !== undefined && "lastError" in reply) {
          chromeStub.runtime.lastError = { message: reply.lastError };
          try {
            cb(undefined);
          } finally {
            chromeStub.runtime.lastError = undefined;
          }
        } else {
          cb(reply);
        }
      });
    },
  };
}

/** Forget everything a previous test left in the world. */
export function resetWorld(): void {
  world.posted = [];
  world.sent = [];
  world.session = {};
  world.local = {};
  world.content = async () => ({ lastError: "no content script" });
  world.injected = [];
  world.inject = async () => {};
}
