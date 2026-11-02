// A fake of the `chrome.tabs` pieces the background's ask loop uses (see
// AskTabApi in src/bridge.ts): one tab, its lifecycle events and a log of
// `tabs.update` calls. Tests drive the tab by hand with `emitUpdated` /
// `emitRemoved` and read the log; nothing talks to a browser.

import type { AskTabApi } from "../../src/bridge";

type UpdatedCb = (tabId: number, info: chrome.tabs.OnUpdatedInfo) => void;
type RemovedCb = (tabId: number) => void;

export interface FakeTabs {
  api: AskTabApi;
  /** Mutable tab record; `get` returns a copy of it. */
  tab: { id: number; status: string; url: string; pendingUrl?: string; title?: string };
  /** `tabs.update` calls, in order. */
  updates: Array<{ tabId: number; props: chrome.tabs.UpdateProperties }>;
  /** What `tabs.update` does besides logging (e.g. start a page load). */
  onUpdate: (props: chrome.tabs.UpdateProperties) => void | Promise<void>;
  /** The tab is gone: `get` rejects. */
  close(): void;
  emitUpdated(info: chrome.tabs.OnUpdatedInfo): void;
  emitRemoved(): void;
  listenerCount(): number;
}

export function fakeTabs(
  tab: { id?: number; status?: string; url?: string } = {},
): FakeTabs {
  const state = {
    id: tab.id ?? 7,
    status: tab.status ?? "complete",
    url: tab.url ?? "https://chatgpt.com/",
  } as FakeTabs["tab"];
  const updated = new Set<UpdatedCb>();
  const removed = new Set<RemovedCb>();
  let gone = false;

  const fake: FakeTabs = {
    tab: state,
    updates: [],
    onUpdate: () => {},
    close() {
      gone = true;
    },
    emitUpdated(info) {
      for (const cb of [...updated]) cb(state.id, info);
    },
    emitRemoved() {
      gone = true;
      for (const cb of [...removed]) cb(state.id);
    },
    listenerCount: () => updated.size + removed.size,
    api: {
      tabs: {
        async get(tabId) {
          if (gone || tabId !== state.id) throw new Error(`No tab with id: ${tabId}.`);
          return { ...state } as unknown as chrome.tabs.Tab;
        },
        async update(tabId, props) {
          fake.updates.push({ tabId, props });
          await fake.onUpdate(props);
          return { ...state } as unknown as chrome.tabs.Tab;
        },
        onUpdated: {
          addListener: (cb) => void updated.add(cb),
          removeListener: (cb) => void updated.delete(cb),
        },
        onRemoved: {
          addListener: (cb) => void removed.add(cb),
          removeListener: (cb) => void removed.delete(cb),
        },
      },
    },
  };
  return fake;
}
