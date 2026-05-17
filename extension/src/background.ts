import {
  newRequestId,
  type ExtensionRequest,
  type ExtensionResponse,
} from "./bridgeProtocol";
import { pingNative } from "./nativeHost";
import { startBridge } from "./bridge";

// Boot the persistent native bridge as soon as the service worker loads.
// Idempotent — safe even if Chrome wakes us multiple times.
startBridge();
chrome.runtime.onStartup.addListener(() => startBridge());
chrome.runtime.onInstalled.addListener(() => startBridge());

const TEST_PROMPT = 'Say "pong" in one short sentence.';
const DEFAULT_TIMEOUT_MS = 120_000;
const CHATGPT_URL_PREFIX = "https://chatgpt.com/";
const BADGE_CLEAR_MS = 4000;

chrome.action.onClicked.addListener((tab) => {
  void runTestAsk(tab);
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
          // Most common cause: content script not injected because the tab was
          // opened before the extension was loaded. Ask the user to reload the
          // ChatGPT tab.
          reject(
            new Error(
              `chrome.runtime.lastError: ${lastError.message ?? "unknown"}. ` +
                "Try reloading the ChatGPT tab after installing/reloading the extension.",
            ),
          );
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

// Expose pingNative on the service-worker global so it can be invoked from the
// SW DevTools console for ad-hoc M4 testing. Not part of any production API;
// later milestones replace this with a proper bridge request type.
(self as unknown as Record<string, unknown>)["pingNative"] = async (
  payload?: unknown,
) => {
  const out = await pingNative(payload === undefined ? {} : { payload });
  console.log("[cgpt-bridge] pingNative:", out);
  return out;
};
