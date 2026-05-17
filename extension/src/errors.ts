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
