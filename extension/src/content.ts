import {
  diagnose,
  getLastAssistantMessageId,
  isSupportedPage,
  setComposerText,
  startKeepAlive,
  stopKeepAlive,
  submit,
  waitForNewAnswer,
} from "./chatgptAdapter";
import {
  toSerializableError,
  UnknownExtensionError,
  UnsupportedPageError,
} from "./errors";
import type {
  ExtensionRequest,
  ExtensionResponse,
} from "./bridgeProtocol";

chrome.runtime.onMessage.addListener(
  (
    message: unknown,
    _sender: chrome.runtime.MessageSender,
    sendResponse: (response: ExtensionResponse) => void,
  ): boolean => {
    const req = parseRequest(message);
    if (!req) {
      sendResponse({
        id: "unknown",
        ok: false,
        errorCode: "bad_request",
        message: "Content script received an unrecognized message shape.",
      });
      return false;
    }

    handleRequest(req)
      .then((response) => sendResponse(response))
      .catch((err: unknown) => {
        const ser = toSerializableError(err);
        sendResponse({
          id: req.id,
          ok: false,
          errorCode: ser.errorCode,
          message: ser.message,
        });
      });

    // Keep the message channel open for the async response.
    return true;
  },
);

function parseRequest(value: unknown): ExtensionRequest | null {
  if (typeof value !== "object" || value === null) return null;
  const v = value as Record<string, unknown>;
  if (typeof v.id !== "string") return null;

  if (
    v.type === "test.ask" &&
    typeof v.text === "string" &&
    typeof v.timeoutMs === "number"
  ) {
    return {
      id: v.id,
      type: "test.ask",
      text: v.text,
      timeoutMs: v.timeoutMs,
    };
  }

  if (v.type === "diagnose") {
    return { id: v.id, type: "diagnose" };
  }

  return null;
}

async function handleRequest(req: ExtensionRequest): Promise<ExtensionResponse> {
  switch (req.type) {
    case "test.ask":
      return handleTestAsk(req);
    case "diagnose":
      return handleDiagnose(req);
    default: {
      const _exhaustive: never = req;
      throw new UnknownExtensionError(
        `Unhandled request: ${JSON.stringify(_exhaustive)}`,
      );
    }
  }
}

async function handleTestAsk(
  req: ExtensionRequest & { type: "test.ask" },
): Promise<ExtensionResponse> {
  if (!isSupportedPage()) {
    throw new UnsupportedPageError(
      `Content script is running on an unsupported host: ${location.host}`,
    );
  }

  // Mark the tab "audible" via a silent OscillatorNode so Chrome stops
  // throttling background timers / mutations while we drive the composer
  // and wait for the streamed reply. Wrap in try/finally so we always
  // release the ref count even on error paths.
  startKeepAlive();
  let text: string;
  try {
    const baselineMessageId = getLastAssistantMessageId();
    await setComposerText(req.text);
    await submit();
    text = await waitForNewAnswer(baselineMessageId, req.timeoutMs);
  } finally {
    stopKeepAlive();
  }

  return { id: req.id, ok: true, kind: "test.ask", text };
}

async function handleDiagnose(
  req: ExtensionRequest & { type: "diagnose" },
): Promise<ExtensionResponse> {
  // diagnose() is intentionally read-only and does NOT require
  // isSupportedPage() — we want it to still report when the host check fails.
  const diagnostics = diagnose();
  return { id: req.id, ok: true, kind: "diagnose", diagnostics };
}
