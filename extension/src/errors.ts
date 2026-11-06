export class CgptBridgeError extends Error {
  readonly code: string;

  constructor(code: string, message: string) {
    super(message);
    this.name = "CgptBridgeError";
    this.code = code;
  }
}

export class UnsupportedPageError extends CgptBridgeError {
  constructor(message = "Active tab is not a supported ChatGPT page.") {
    super("unsupported_page", message);
    this.name = "UnsupportedPageError";
  }
}

export class ComposerNotFoundError extends CgptBridgeError {
  constructor(message = "ChatGPT composer input was not found in the DOM.") {
    super("composer_not_found", message);
    this.name = "ComposerNotFoundError";
  }
}

export class SendButtonNotFoundError extends CgptBridgeError {
  constructor(message = "ChatGPT send button was not found or is disabled.") {
    super("send_button_not_found", message);
    this.name = "SendButtonNotFoundError";
  }
}

export class AnswerTimeoutError extends CgptBridgeError {
  constructor(message = "Timed out waiting for a stable assistant response.") {
    super("answer_timeout", message);
    this.name = "AnswerTimeoutError";
  }
}

export class AnswerNotDetectedError extends CgptBridgeError {
  constructor(message = "No reply was detected after submitting the prompt.") {
    super("answer_not_detected", message);
    this.name = "AnswerNotDetectedError";
  }
}

// The ask's files did not get into the composer, or did not finish uploading,
// in time; the prompt was not sent. The wire has no code of its own for it
// (it goes as `dom_failure`), so the message starts with this one.
export class AttachmentUploadFailedError extends CgptBridgeError {
  constructor(message = "The attached files did not upload in ChatGPT; the prompt was not sent.") {
    super("attachment_upload_failed", `attachment_upload_failed: ${message}`);
    this.name = "AttachmentUploadFailedError";
  }
}

// The own tab's composer holds files this ask did not put there (left by a
// failed ask) and they could not be removed: Send would carry them into this
// chat. Refused before anything was typed; `dom_failure` on the wire, so the
// message starts with the code.
export class AttachmentsLeftOverError extends CgptBridgeError {
  constructor(
    message = "The composer holds files of an earlier ask that could not be removed; the prompt was not sent. Remove them in the cgpt-bridge window.",
  ) {
    super("attachments_left_over", `attachments_left_over: ${message}`);
    this.name = "AttachmentsLeftOverError";
  }
}

export class StopButtonNotFoundError extends CgptBridgeError {
  constructor(message = "ChatGPT is generating but its Stop button was not found.") {
    super("stop_button_not_found", message);
    this.name = "StopButtonNotFoundError";
  }
}

export class RateLimitedError extends CgptBridgeError {
  constructor(message = "ChatGPT is rate limiting this account (HTTP 429 on the answer stream).") {
    super("rate_limited", message);
    this.name = "RateLimitedError";
  }
}

export class BadTargetError extends CgptBridgeError {
  constructor(message = "Target is not a chatgpt.com conversation URL.") {
    super("bad_target", message);
    this.name = "BadTargetError";
  }
}

export class TargetNotReachedError extends CgptBridgeError {
  constructor(message = "The target chat could not be opened.") {
    super("target_not_reached", message);
    this.name = "TargetNotReachedError";
  }
}

export class ChatUrlMissingError extends CgptBridgeError {
  constructor(message = "The chat has no saved chatgpt.com address after the answer.") {
    super("chat_url_missing", message);
    this.name = "ChatUrlMissingError";
  }
}

export class TargetMismatchError extends CgptBridgeError {
  constructor(message = "The page no longer shows the target chat.") {
    super("target_mismatch", message);
    this.name = "TargetMismatchError";
  }
}

export class NotLoggedInError extends CgptBridgeError {
  constructor(message = "ChatGPT в окне cgpt-bridge не выполнил вход — войдите в этом окне") {
    super("not_logged_in", message);
    this.name = "NotLoggedInError";
  }
}

export class PreviousAnswerRunningError extends CgptBridgeError {
  constructor(message = "previous answer still generating") {
    super("previous_answer_running", message);
    this.name = "PreviousAnswerRunningError";
  }
}

export class AskRunningError extends CgptBridgeError {
  constructor(message = "another ask is running in this tab") {
    super("ask_running", message);
    this.name = "AskRunningError";
  }
}

// A stop reaches the host as the code `cancelled`. A host older than that code
// gets `internal` with one of these two messages instead (forOlderHost in
// bridgeProtocol.ts), and the gateway still turns them into "stopped by the
// user" (HTTP 422) by exact match (gateway/src/backend.rs);
// protocol/tests/fixtures/stop_messages.txt pins them on both sides.
export const CANCELLED_BY_STOP_MESSAGE = "cancelled by stop";
export const STOPPED_IN_PAGE_MESSAGE = "The answer was stopped in the ChatGPT page before it ended.";

export class CancelledError extends CgptBridgeError {
  constructor(message = CANCELLED_BY_STOP_MESSAGE) {
    super("cancelled", message);
    this.name = "CancelledError";
  }
}

// The extension was reloaded or updated while this content script drove an
// ask: the script is cut off ("Extension context invalidated") and a new one
// runs next to it. Nobody receives this error; it only ends the old ask.
export class ExtensionReloadedError extends CgptBridgeError {
  constructor(
    message = "The extension was reloaded or updated during this ask; this page's old content script types and sends nothing more.",
  ) {
    super("extension_reloaded", message);
    this.name = "ExtensionReloadedError";
  }
}

export class ContentScriptNotReadyError extends CgptBridgeError {
  constructor(message = "Content script did not respond on the active ChatGPT tab.") {
    super("content_script_not_ready", message);
    this.name = "ContentScriptNotReadyError";
  }
}

export class UnknownExtensionError extends CgptBridgeError {
  constructor(message = "Unknown extension-internal error.") {
    super("unknown", message);
    this.name = "UnknownExtensionError";
  }
}

export interface SerializableError {
  errorCode: string;
  message: string;
}

export function toSerializableError(err: unknown): SerializableError {
  if (err instanceof CgptBridgeError) {
    return { errorCode: err.code, message: err.message };
  }
  if (err instanceof Error) {
    return { errorCode: "unknown", message: err.message };
  }
  return { errorCode: "unknown", message: String(err) };
}
