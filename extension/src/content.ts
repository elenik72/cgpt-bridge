import { submitBudgetMs, takeStop, type PageAskState } from "./askControl";
import {
  assertAnsweredInTarget,
  assertAtTarget,
  captureAnswerBaseline,
  currentConversationUrl,
  diagnose,
  findComposer,
  forgetProject,
  isLoggedIn,
  isSupportedPage,
  lastTurnCancelled,
  openProjectForNewChat,
  openTarget,
  previousAnswerRunning,
  generatingSignal,
  projectPageId,
  setComposerText,
  settlePreviousAnswer,
  setPageShimActive,
  startKeepAlive,
  stopGenerating,
  stopKeepAlive,
  submit,
  targetMatchesPath,
  userTurnCount,
  waitForLoggedIn,
  waitForNewAnswer,
  waitForSavedChatUrl,
  type PageTarget,
} from "./chatgptAdapter";
import {
  AnswerNotDetectedError,
  AnswerTimeoutError,
  ChatUrlMissingError,
  AskRunningError,
  CancelledError,
  ExtensionReloadedError,
  NotLoggedInError,
  PreviousAnswerRunningError,
  STOPPED_IN_PAGE_MESSAGE,
  TargetMismatchError,
  TargetNotReachedError,
  toSerializableError,
  UnknownExtensionError,
  UnsupportedPageError,
} from "./errors";
import {
  isValidProjectName,
  type ChatTarget,
  type ExtensionRequest,
  type ExtensionResponse,
} from "./bridgeProtocol";

// After a targeted answer, how long to wait for the temporary
// `/c/local-chatgpt:…` URL of a new chat to turn into the saved one.
const CONVERSATION_URL_WAIT_MS = 5_000;
// A Stop answers within this (the host waits 10 s for it).
const STOP_BUDGET_MS = 5_000;
// A gateway ask that failed after its prompt was sent (deadline passed,
// no reply) stops the answer within this — the host waits 5 s past the
// deadline for the error.
const ABANDON_STOP_MS = 4_000;
// Right after the deadline the answer may only just be starting.
const ABANDON_START_GRACE_MS = 1_000;
// A CLI ask into a tab whose answer may be just ending waits up to this long
// (never past the time it needs to send) before it refuses the tab as busy.
const ANSWER_ENDING_GRACE_MS = 15_000;
// A gateway ask into a page that reads as signed out waits this long for it
// to read as signed in (a page still loading) before it refuses the turn.
const LOGIN_GRACE_MS = 2_000;
// While a gateway ask waits for its answer, a page away from the target
// conversation this long ends the ask (target_mismatch). Shorter visits, and
// ChatGPT moving the address for a moment, are tolerated.
const LEFT_TARGET_GRACE_MS = 3_000;

// The ask running in this page. The background sends at most one at a time
// per tab; a second one (toolbar test, or a retry after a service-worker
// restart) is refused while it runs. `stop` reads and cancels it.
let currentAsk: PageAskState | null = null;

// Set when a gateway ask failed because the page left its chat (before
// Send or after it); cleared by the next ask. Meanwhile a stop for that turn presses nothing:
// what generates now is the chat the user switched to, and the next ask's
// settlePreviousAnswer stops the bridge's own answer in its own chat.
let stopBlockedByMismatch = false;

// The chat of the newest gateway ask (set when it starts, null after an
// untargeted one). Stop is pressed for that ask, or for its turn after it
// ended, only while the page shows this chat (`assertShowsTurn`): the user
// may have switched the window to a chat of their own that is generating.
let lastTurn: SentTurn | null = null;

interface SentTurn {
  // As reached: a new chat may be in a ChatGPT project.
  target: PageTarget;
  prompt: string;
}

// The chat of the running CLI ask (no target), set at Send. A `stop_ask`
// for it (Ctrl+C in `cgpt ask`, in the user's own tab) presses Stop only
// while the page shows this chat, by the same rule as a gateway ask's
// (`assertShowsTurn`): the saved chat it was sent from by its path; else
// (sent from `/`, a project's page or an unsaved chat) the new chat by its
// one user turn holding the prompt, inside the project it was sent from.
let cliTurn: { ask: PageAskState; turn: SentTurn } | null = null;

// Set by a stop for the running ask that pressed nothing (the page shows
// another chat): the ask ends with this error at its next wait tick instead
// of holding the tab until its deadline. Cleared by the next ask.
let endRunningAsk: { ask: PageAskState; err: Error } | null = null;

// Why Stop is not pressed for a turn. `mismatch`: the page shows another
// chat. Null: a new chat whose user turn the markup does not show (drift),
// which is not known to be another chat.
class StopRefused extends Error {
  constructor(
    readonly mismatch: TargetMismatchError | null,
    reason: string,
  ) {
    super(reason);
    this.name = "StopRefused";
  }
}

// Throws StopRefused unless the page shows `turn`'s chat: the saved
// conversation by its path, a new chat by its one user turn holding the prompt.
function assertShowsTurn(turn: SentTurn): void {
  try {
    assertAnsweredInTarget(turn.target, turn.prompt, "before Stop was pressed");
  } catch (err) {
    if (!(err instanceof TargetMismatchError)) throw err;
    if (turn.target.kind === "new_chat" && userTurnCount() === 0) {
      throw new StopRefused(
        null,
        `no user turn is recognised in the new chat at ${location.pathname} (page markup changed?).`,
      );
    }
    throw new StopRefused(err, err.message);
  }
}

function warnNotPressed(reason: string): void {
  console.warn("[cgpt-bridge] Stop not pressed:", reason);
}

// The own window's mark (targetTab.ts) in the page's sessionStorage: kept
// across reloads of the page and of the extension, and restored with the
// window after a browser restart; a new tab does not have it.
const OWN_MARK_KEY = "cgptBridgeOwnWindow";

function readOwnMark(): string | null {
  try {
    const mark = sessionStorage.getItem(OWN_MARK_KEY);
    return mark === null || mark === "" ? null : mark;
  } catch {
    return null; // storage blocked: no mark, the safe side
  }
}

function writeOwnMark(mark: string): void {
  try {
    sessionStorage.setItem(OWN_MARK_KEY, mark);
  } catch (err) {
    console.warn("[cgpt-bridge] could not mark the gateway window's page:", err);
  }
}

// Marks the isolated world this script runs in. The background injects the
// script again into pages that may already run it (reinject.ts), and the
// manifest may inject it into a page the background injected first; a
// second run shares this world's `window` (not the module state) and must
// not add a second listener: each request would be handled twice. A
// reloaded or updated extension runs in a new isolated world without the
// mark, next to the cut-off listener of the old one.
const INSTALLED_MARK = "__cgptBridgeContentScript";

/** Adds the message listener unless this world has one; true when it did. */
export function installContentScript(): boolean {
  const w = globalThis as unknown as Record<string, unknown>;
  if (w[INSTALLED_MARK] === true) return false;
  w[INSTALLED_MARK] = true;
  chrome.runtime.onMessage.addListener(onMessage);
  return true;
}

installContentScript();

// False once the extension was reloaded or updated: this script is then cut
// off ("Extension context invalidated", `chrome.runtime.id` is gone) but goes
// on running, and a new content script is put into the page next to it.
function extensionContextAlive(): boolean {
  try {
    return typeof chrome.runtime?.id === "string" && chrome.runtime.id !== "";
  } catch {
    return false;
  }
}

function onMessage(
  message: unknown,
  _sender: chrome.runtime.MessageSender,
  sendResponse: (response: ExtensionResponse) => void,
): boolean {
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
}

function parseRequest(value: unknown): ExtensionRequest | null {
  if (typeof value !== "object" || value === null) return null;
  const v = value as Record<string, unknown>;
  if (typeof v.id !== "string") return null;

  if (
    v.type === "test.ask" &&
    typeof v.text === "string" &&
    typeof v.timeoutMs === "number"
  ) {
    const target = parseTarget(v.target);
    if (target === null) return null;
    if (v.project !== undefined && !isValidProjectName(v.project)) return null;
    return {
      id: v.id,
      type: "test.ask",
      text: v.text,
      timeoutMs: v.timeoutMs,
      ...(target === undefined ? {} : { target }),
      // Compared with the page's labels as NFC (a macOS folder name is often NFD).
      ...(typeof v.project === "string" ? { project: v.project.normalize("NFC") } : {}),
      ...(typeof v.deadlineMs === "number" ? { deadlineMs: v.deadlineMs } : {}),
      ...(v.reloaded === true ? { reloaded: true } : {}),
      ...(typeof v.ownMark === "string" && v.ownMark !== "" ? { ownMark: v.ownMark } : {}),
    };
  }

  if (v.type === "stop" && (v.askId === undefined || typeof v.askId === "string")) {
    return {
      id: v.id,
      type: "stop",
      ...(typeof v.askId === "string" ? { askId: v.askId } : {}),
    };
  }

  if (v.type === "diagnose" || v.type === "status" || v.type === "own_mark") {
    return { id: v.id, type: v.type };
  }

  return null;
}

// undefined: no target (untargeted ask); null: malformed target.
function parseTarget(value: unknown): ChatTarget | undefined | null {
  if (value === undefined || value === null) return undefined;
  if (typeof value !== "object") return null;
  const t = value as Record<string, unknown>;
  if (t.kind === "new_chat") return { kind: "new_chat" };
  if (t.kind === "conversation" && typeof t.url === "string") {
    return { kind: "conversation", url: t.url };
  }
  return null;
}

async function handleRequest(req: ExtensionRequest): Promise<ExtensionResponse> {
  switch (req.type) {
    case "test.ask":
      return handleTestAsk(req);
    case "diagnose":
      return handleDiagnose(req);
    case "stop":
      return handleStop(req);
    case "status":
      return handleStatus(req);
    case "own_mark":
      return { id: req.id, ok: true, kind: "own_mark", mark: readOwnMark() };
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

  if (currentAsk !== null && !currentAsk.done) throw new AskRunningError();

  const deadline = req.deadlineMs ?? Date.now() + req.timeoutMs;
  const left = (): number => Math.max(0, deadline - Date.now());
  const target = req.target;
  // Only a gateway ask runs in the own tab.
  if (target !== undefined && req.ownMark !== undefined) writeOwnMark(req.ownMark);
  const ask: PageAskState = { id: req.id, cancelled: false, submitted: false, done: false };
  currentAsk = ask;
  stopBlockedByMismatch = false;
  endRunningAsk = null;
  cliTurn = null;
  // The target as reached (a new chat may be opened in a project).
  let pageTarget: PageTarget | undefined = target;
  let turn: SentTurn | null = target === undefined ? null : { target, prompt: req.text };
  lastTurn = turn;
  // Set when the new chat was asked into a project and went to a plain one.
  let warning: string | null = null;
  try {
    // Gateway asks name their chat: switch to it first. If that needs a
    // full page load, the background does it and asks again.
    if (target !== undefined) {
      const opened = await openTarget(target, {
        deadline,
        reloaded: req.reloaded === true,
        ...(req.project === undefined ? {} : { project: req.project }),
      }).catch(
        (err: unknown) => {
          // An account's saved chat never opens in a signed-out window: say
          // why. A page with no composer at all is not known to be signed out.
          if (err instanceof TargetNotReachedError && findComposer() !== null && !isLoggedIn()) {
            throw new NotLoggedInError();
          }
          throw err;
        },
      );
      if (!opened.ready) return { id: req.id, ok: true, kind: "navigate", url: opened.url };
      const reached = opened.target;
      pageTarget = reached;
      turn = { target: reached, prompt: req.text };
      lastTurn = turn;
      warning = opened.warning ?? null;
      // A signed-out page would send the prompt to the anonymous ChatGPT,
      // outside the user's account: refuse before anything is typed.
      if (!(await waitForLoggedIn(Math.min(LOGIN_GRACE_MS, left())))) throw new NotLoggedInError();
      // An earlier answer (say, one whose ask timed out) may still be
      // generating here; never type into a chat that is mid-answer.
      await settlePreviousAnswer(Math.min(STOP_BUDGET_MS, left()), () => {
        try {
          assertAtTarget(reached);
        } catch (err) {
          if (err instanceof TargetMismatchError) warnNotPressed(err.message);
          throw err;
        }
      });
    } else if (
      await previousAnswerRunning(
        Math.min(ANSWER_ENDING_GRACE_MS, Math.max(0, left() - submitBudgetMs(req.timeoutMs))),
      )
    ) {
      // A CLI ask types into the user's own tab: never press Stop in it, and
      // never type into an answer that is still generating (the reply would
      // be that answer's, not ours).
      throw new PreviousAnswerRunningError(
        "The previous answer is still generating in this tab; wait for it to finish or press Stop in the ChatGPT tab, then ask again " +
          `(page sign: ${generatingSignal() ?? "none"}).`,
      );
    } else if (req.project !== undefined) {
      // A CLI ask that opens a new chat puts it into its project; one into
      // a chat continues it there.
      warning = await openProjectForNewChat(req.project, deadline);
    }

    // Run right before typing and right before every send click: nothing
    // may reach the chat after a Stop, too close to the deadline, in a page
    // that has left the target chat, or from a script an extension reload
    // cut off (the new one may be typing into the same composer).
    const checkpoint = (): void => {
      if (!extensionContextAlive()) throw new ExtensionReloadedError();
      if (ask.cancelled) throw new CancelledError();
      if (left() < submitBudgetMs(req.timeoutMs)) {
        throw new AnswerTimeoutError(
          `Only ${left()} ms left before the deadline; the prompt was not sent.`,
        );
      }
      if (pageTarget !== undefined) assertAtTarget(pageTarget);
    };

    // The chat a CLI ask (no target) was sent from, when it was a saved one:
    // set at Send. It is watched like a gateway ask's conversation. A new
    // chat has no path yet, and the page legitimately moves it to `/c/<id>`.
    let ownChat: Extract<PageTarget, { kind: "conversation" }> | undefined;
    const watchedChat = (): PageTarget | undefined =>
      pageTarget?.kind === "conversation" ? pageTarget : ownChat;
    // Fails unless the page still shows the watched chat. `when` names the
    // moment in the error.
    const assertInWatchedChat = (when?: string): void => {
      if (ownChat !== undefined && pageTarget === undefined) {
        // `ownChat` is set at Send: the prompt is in that chat, and ChatGPT
        // goes on answering it there after the tab left (checked live).
        if (!targetMatchesPath(ownChat, location.pathname)) {
          throw new TargetMismatchError(
            `The chat the prompt was sent to was closed or deleted, or the tab left it ${when ?? "before the answer was read"}: ` +
              `at ${location.pathname}, want ${ownChat.url}. ` +
              `The prompt was sent: unless that chat was deleted, ChatGPT goes on answering it there; ` +
              `open ${ownChat.url} to read the answer (asking again would send it twice).`,
          );
        }
      } else if (pageTarget?.kind === "conversation") {
        assertAnsweredInTarget(pageTarget, req.text, when);
      }
    };

    // Run on every tick of the answer wait: a refused stop ends the ask, and
    // so does a page that stays away from the watched chat (a gateway ask's
    // conversation, or the saved chat a CLI ask was sent from; a new chat has
    // no fixed path to watch, it is checked once the answer is in).
    let awaySince = 0;
    const watchWait = (): void => {
      if (endRunningAsk?.ask === ask) throw endRunningAsk.err;
      const watched = watchedChat();
      if (watched === undefined) return;
      if (targetMatchesPath(watched, location.pathname)) {
        awaySince = 0;
      } else if (awaySince === 0) {
        awaySince = Date.now();
      } else if (Date.now() - awaySince >= LEFT_TARGET_GRACE_MS) {
        assertInWatchedChat("while the answer was awaited");
      }
    };

    // Mark the tab "audible" via a silent OscillatorNode so Chrome stops
    // throttling background timers / mutations while we drive the composer
    // and wait for the streamed reply. Wrap in try/finally so we always
    // release the ref count even on error paths.
    startKeepAlive();
    // Grace on top of the deadline so the shim outlives our own wait. Only
    // the shim's reports in this epoch belong to this ask.
    const shimEpoch = await setPageShimActive(true, left() + 15_000);
    let text: string;
    try {
      const baseline = captureAnswerBaseline();
      checkpoint();
      await setComposerText(req.text);
      const earlyShimReports = await submit(() => {
        checkpoint();
        ask.submitted = true;
        if (target === undefined) {
          const url = currentConversationUrl();
          if (url !== null) ownChat = { kind: "conversation", url };
          const projectId = projectPageId(location.pathname);
          const sentTo: PageTarget =
            ownChat ?? (projectId === null ? { kind: "new_chat" } : { kind: "new_chat", projectId });
          cliTurn = { ask, turn: { target: sentTo, prompt: req.text } };
        }
      }, shimEpoch, deadline);
      // Gateway asks never return a cut-off answer at the deadline.
      text = await waitForNewAnswer(baseline, left(), {
        partialOnTimeout: target === undefined,
        shimEpoch,
        earlyShimReports,
        check: watchWait,
        stopped: () => ask.cancelled,
      }).catch((err: unknown) => {
        // No answer (say, the stream ended without one) in a page that has
        // left the target chat: the cause is the switch, and the stop guard
        // below must hold for it. Only a conversation has a path to check; a
        // new chat's turn count also fails on drifted markup (0 turns).
        if (watchedChat() !== undefined && err instanceof AnswerNotDetectedError) {
          assertInWatchedChat();
        }
        throw err;
      });
    } catch (err) {
      // A gateway turn that failed after its prompt was sent must not keep
      // generating: the next ask in this chat would type into it. Unless the
      // page left its chat: what generates now is the user's other chat
      // (abandonTurn then fails the ask as target_mismatch instead).
      if (turn !== null && ask.submitted && !(err instanceof TargetMismatchError)) {
        await abandonTurn(err, turn);
      }
      throw err;
    } finally {
      void setPageShimActive(false);
      stopKeepAlive();
    }

    // A CLI ask from a saved chat returns only an answer read in that chat.
    if (ownChat !== undefined) assertInWatchedChat();

    // A targeted ask always names its chat (or fails): null reaches the
    // gateway only from an extension that predates targets.
    const conversationUrl =
      pageTarget !== undefined
        ? await waitForSavedChatUrl(Math.min(CONVERSATION_URL_WAIT_MS, left()), pageTarget).catch(
            async (err: unknown) => {
              // A project whose chats get no address: its remembered id is
              // not used again (a dead page may still show a composer).
              const sent = pageTarget;
              if (
                err instanceof ChatUrlMissingError &&
                sent?.kind === "new_chat" &&
                sent.projectId !== undefined &&
                req.project !== undefined
              ) {
                await forgetProject(req.project, sent.projectId);
              }
              throw err;
            },
          )
        : currentConversationUrl();
    // A gateway turn returns only a whole answer from its own chat. Neither
    // failure stops anything: the page may show the user's other chat, and
    // a stopped turn is not generating.
    if (pageTarget !== undefined) {
      assertAnsweredInTarget(pageTarget, req.text);
      // Stopped by the host: the text is cut off, never a gateway answer.
      if (lastTurnCancelled() || ask.cancelled) {
        throw new CancelledError(STOPPED_IN_PAGE_MESSAGE);
      }
    }
    return {
      id: req.id,
      ok: true,
      kind: "test.ask",
      text,
      conversationUrl,
      ...(warning === null ? {} : { warning }),
    };
  } catch (err) {
    // Before Send or after the answer alike: the page shows another chat.
    if (err instanceof TargetMismatchError) stopBlockedByMismatch = true;
    throw err;
  } finally {
    ask.done = true;
    if (currentAsk === ask) currentAsk = null;
  }
}

// Stops the failed turn's answer if the page still shows its chat. When it
// shows another chat, presses nothing and throws TargetMismatchError: a
// conversation is checked by its path at once, any chat right before the
// click. A new chat is not checked up front, and one whose user turn is not
// recognised (drifted markup) keeps the ask's own error: that failure is the
// DOM's, not a switch.
async function abandonTurn(cause: unknown, turn: SentTurn): Promise<void> {
  await setPageShimActive(false);
  try {
    if (turn.target.kind === "conversation") assertShowsTurn(turn);
    await stopGenerating(
      ABANDON_STOP_MS,
      cause instanceof AnswerTimeoutError ? ABANDON_START_GRACE_MS : 0,
      () => assertShowsTurn(turn),
    );
  } catch (err) {
    if (err instanceof StopRefused) {
      warnNotPressed(err.message);
      if (err.mismatch !== null) throw err.mismatch;
      return;
    }
    console.warn("[cgpt-bridge] could not stop the abandoned answer:", err);
  }
}

// stopGenerating for a stop from the host, guarded by `turn` (none for an
// untargeted ask). A page that does not show the turn's chat gets no click:
// the refusal is returned.
async function stopForHost(
  startGraceMs: number | undefined,
  turn: SentTurn | null,
): Promise<boolean | StopRefused> {
  try {
    return await stopGenerating(STOP_BUDGET_MS, startGraceMs, () => {
      if (turn !== null) assertShowsTurn(turn);
    });
  } catch (err) {
    if (!(err instanceof StopRefused)) throw err;
    warnNotPressed(err.message);
    return err;
  }
}

async function handleStop(
  req: ExtensionRequest & { type: "stop" },
): Promise<ExtensionResponse> {
  const ask = currentAsk;
  const reply = (wasGenerating: boolean, otherChat = false, notSent = false): ExtensionResponse => ({
    id: req.id,
    ok: true,
    kind: "stop",
    wasGenerating,
    ...(otherChat ? { otherChat: true as const } : {}),
    ...(notSent ? { notSent: true as const } : {}),
  });
  // Marks a running ask cancelled: from here on it sends nothing.
  switch (takeStop(ask, req.askId)) {
    case "cancel":
      // Not sent yet: the ask aborts at its next checkpoint ("cancelled by
      // stop"). The shim stays as it is — it belongs to that ask.
      return reply(false, false, true);
    case "press_for_ask":
      // Sent and still running. While active the shim swallows the page's
      // stop request, so put it to sleep before pressing Stop; the ask then
      // finishes with whatever was generated so far (or fails "cancelled by
      // stop" if it was still confirming its Send click; a gateway ask whose
      // turn the page marks `cancelled` fails "cancelled" too).
      // Unless the page does not show the ask's chat: then nothing is
      // pressed and the ask, cancelled, ends at once (target_mismatch, or
      // "cancelled by stop" when drifted markup hides its user turn). A
      // CLI ask's stop says so (`otherChat`): the user is told the answer
      // was not stopped because the tab shows another chat.
      if (ask === null) return reply(false);
      await setPageShimActive(false);
      {
        const cli = cliTurn !== null && cliTurn.ask === ask ? cliTurn.turn : null;
        const result = await stopForHost(undefined, cli ?? lastTurn);
        if (!(result instanceof StopRefused)) return reply(true);
        endRunningAsk = { ask, err: result.mismatch ?? new CancelledError() };
        return reply(false, cli !== null);
      }
    case "press_if_generating":
      // No ask here any more, but its answer may still be generating —
      // unless the page left that answer's chat (see stopBlockedByMismatch,
      // lastTurn). With no gateway ask known, there is no chat to stop.
      if (stopBlockedByMismatch) return reply(false);
      if (lastTurn === null) {
        console.warn("[cgpt-bridge] Stop not pressed: no gateway ask has run in this page.");
        return reply(false);
      }
      await setPageShimActive(false);
      return reply((await stopForHost(0, lastTurn)) === true);
    case "ignore":
      return reply(false);
  }
}

async function handleStatus(
  req: ExtensionRequest & { type: "status" },
): Promise<ExtensionResponse> {
  return {
    id: req.id,
    ok: true,
    kind: "status",
    loggedIn: isLoggedIn(),
    conversationUrl: currentConversationUrl(),
  };
}

async function handleDiagnose(
  req: ExtensionRequest & { type: "diagnose" },
): Promise<ExtensionResponse> {
  // diagnose() is intentionally read-only and does NOT require
  // isSupportedPage() — we want it to still report when the host check fails.
  const diagnostics = diagnose();
  return { id: req.id, ok: true, kind: "diagnose", diagnostics };
}
