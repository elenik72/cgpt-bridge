import {
  newRequestId,
  type ExtensionRequest,
  type ExtensionResponse,
} from "./bridgeProtocol";
import { noteBrowserStart, startBridge } from "./bridge";
import { injectIntoOpenChatTabs } from "./reinject";

// Boot the persistent native bridge as soon as the service worker loads.
// Idempotent — safe even if Chrome wakes us multiple times.
startBridge();
chrome.runtime.onStartup.addListener(() => {
  startBridge();
  // A gateway window Chrome restored comes back with new ids.
  void noteBrowserStart();
});
chrome.runtime.onInstalled.addListener((details) => {
  startBridge();
  // ChatGPT tabs open from before this install or update have no content
  // script that reaches this version.
  if (details.reason === "install" || details.reason === "update") {
    void injectIntoOpenChatTabs(chrome);
  }
});

const TEST_PROMPT = 'Say "pong" in one short sentence.';
const DEFAULT_TIMEOUT_MS = 120_000;
const CHATGPT_URL_PREFIX = "https://chatgpt.com/";
const BADGE_CLEAR_MS = 4000;

// Developer flag: a toolbar click sends a real test prompt into the ChatGPT
// tab, i.e. a message in the user's account. It does that only when this key
// is true in chrome.storage.local (set it from the service-worker console:
// `chrome.storage.local.set({ devTestAsk: true })`); otherwise a click does
// nothing.
const DEV_TEST_ASK_KEY = "devTestAsk";

chrome.action.onClicked.addListener((tab) => {
  void (async () => {
    const stored = await chrome.storage.local.get(DEV_TEST_ASK_KEY);
    if (stored[DEV_TEST_ASK_KEY] !== true) {
      console.log(
        `[cgpt-bridge] toolbar click ignored: set chrome.storage.local ${DEV_TEST_ASK_KEY}=true to send a test prompt`,
      );
      return;
    }
    await runTestAsk(tab);
  })().catch((err) => {
    console.error("[cgpt-bridge] toolbar click failed:", err);
  });
});

async function runTestAsk(tab: chrome.tabs.Tab): Promise<void> {
  try {
    await setBadge("...", "#808080");

    if (typeof tab.id !== "number") {
      throw new Error("Active tab has no id.");
    }
    if (typeof tab.url !== "string" || !tab.url.startsWith(CHATGPT_URL_PREFIX)) {
      throw new Error(
        `Active tab is not on ${CHATGPT_URL_PREFIX}. Current URL: ${tab.url ?? "(unknown)"}`,
      );
    }

    const request: ExtensionRequest = {
      id: newRequestId(),
      type: "test.ask",
      text: TEST_PROMPT,
      timeoutMs: DEFAULT_TIMEOUT_MS,
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
        response.message,
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

function sendToTab(
  tabId: number,
  request: ExtensionRequest,
): Promise<ExtensionResponse> {
  return new Promise((resolve, reject) => {
    chrome.tabs.sendMessage(
      tabId,
      request,
      (response: ExtensionResponse | undefined) => {
        const lastError = chrome.runtime.lastError;
        if (lastError) {
          reject(new Error(`chrome.runtime.lastError: ${lastError.message ?? "unknown"}`));
          return;
        }
        if (!response) {
          reject(new Error("Empty response from content script."));
          return;
        }
        resolve(response);
      },
    );
  });
}

async function setBadge(text: string, color: string): Promise<void> {
  await chrome.action.setBadgeBackgroundColor({ color });
  await chrome.action.setBadgeText({ text });
}

async function clearBadge(): Promise<void> {
  await chrome.action.setBadgeText({ text: "" });
}
