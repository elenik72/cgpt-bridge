// PROJ: a new chat of the bridge goes into the ChatGPT project named after the
// working folder (research/2026-10-28-chatgpt-projects-live.md), on the fake
// window with snapshot HTML (the adapter's selectors are evaluated against
// the attributes). Synthetic markup and text only. The sidebar holds the
// project rows and the create button; the page itself (navigation, the
// create dialog, the answer) is played by the test.

import { beforeEach, test } from "node:test";
import assert from "node:assert/strict";

import { ask, clearLocalStore, doc, goTo, loadPage, localStore, resetPage, win } from "./harness/fakeWindow";
import type { FakeElement } from "./harness/fakeWindow";
import { loadSnapshot, parseHtml } from "./harness/domSnapshot";
import { later, settlePage } from "./harness/chatPage";
import { useAutoClock } from "./harness/clock";
import {
  conversationPathFrom,
  projectPageId,
  targetMatchesPath,
} from "../src/chatgptAdapter";

await loadPage();
useAutoClock();
beforeEach(() => clearLocalStore());

// The project ids the page remembers (chrome.storage.local).
const remembered = (): unknown => localStore["chatgptProjectIds"];
const remember = (ids: Record<string, string>): void => {
  localStore["chatgptProjectIds"] = ids;
};

const OTHER_ID = "g-p-fedcba9876543210fedcba9876543210";
const PROJ_ID = "g-p-0123456789abcdef0123456789abcdef";
const NEW_ID = "g-p-00112233445566778899aabbccddeeff";
const NAME = "my-proj";
const PROMPT = "What is two plus two?";

// As recorded live on 2026-10-28: the row holds an expand button, an actions
// button (with a menu) and a "new chat in <project>" button; clicking the row
// itself, or its expand button, only expands the project's chat list.
const row = (name: string, id: string): string =>
  `<div role="button" data-app-action-sidebar-project-row="" data-app-action-sidebar-project-label="${name}" ` +
  `data-app-action-sidebar-project-id="${id}" data-app-action-sidebar-project-collapsed="true">` +
  `<button type="button" aria-label="Развернуть ${name}" data-test-expand=""></button>${name}` +
  `<button type="button" aria-haspopup="menu" aria-label="Действия проекта ${name}" data-test-actions=""></button>` +
  `<button type="button" aria-label="Новый чат в ${name}" data-test-new-chat=""></button></div>`;

// As recorded live on 2026-10-28: the create button sits inside the "Projects"
// header's menu trigger, `[aria-expanded=false][data-state=closed]` while the
// rows are shown; that attribute is the menu's, not the section's.
// With more than five projects (live check 2026-10-28) the list shows the five
// newest and a "show more" item; `more` adds that item.
function page(opts: { rows?: string; create?: boolean; more?: boolean } = {}): string {
  const create = opts.create === false ? "" : `<button type="button" data-app-action-sidebar-project-create="" aria-label="Добавить новый проект"></button>`;
  return `
<nav>
  <button type="button" data-sidebar-cycle-item="" aria-label="Новый чат"></button>
  <div data-projects-section="">
    <div data-header-menu="" aria-expanded="false" data-state="closed" data-disabled="">
      <div data-chatgpt-project-conversation-drop-target="">${create}</div>
    </div>
    <div role="list" data-project-list="">
      <div role="listitem" data-project-rows="">${opts.rows ?? ""}</div>${
        opts.more === true
          ? `<div role="listitem"><button type="button" data-test-show-more="">Показать больше</button></div>`
          : ""
      }
    </div>
  </div>
</nav>
<main>
  <div data-thread=""></div>
  <form>
    <div contenteditable="true" role="textbox" data-composer-markdown="" aria-label="Спросить ChatGPT"></div>
    <button type="submit" aria-label="Send prompt"></button>
  </form>
</main>`;
}

function el(selector: string): FakeElement {
  const found = doc.querySelector(selector);
  if (!found) throw new Error(`not on the page: ${selector}`);
  return found;
}

const userTurn = (text: string): string =>
  `<section data-talvt-turn-state="complete"><div data-user-message-bubble=""><div>${text}</div></div></section>`;

const answerTurn = (id: string, state: string, text: string): string =>
  `<section data-talvt-turn-state="${state}"><div data-dil-source-message-id="${id}">` +
  `<div data-markdown-copy="">${text === "" ? "" : `<p data-d-text-flow="">${text}</p>`}</div></div></section>`;

interface Played {
  sent: string[];
  // Paths the page was at when Send was clicked.
  sentAt: string[];
}

/**
 * The page at `path` with `sidebar`. A Send click takes the prompt, moves the
 * page to `chatPath(where it was)` and shows the answer "Four." after 300 ms.
 */
function showPage(
  path: string,
  sidebar: Parameters<typeof page>[0],
  chatPath: (from: string) => string,
  past = "",
): Played {
  resetPage(path);
  loadSnapshot(page(sidebar));
  const thread = el("[data-thread]");
  thread.append(...parseHtml(past));
  const composer = el('[role="textbox"]');
  const played: Played = { sent: [], sentAt: [] };
  el('main button[type="submit"]').onClick = () => {
    const prompt = composer.textContent;
    if (prompt.trim() === "") return;
    played.sent.push(prompt);
    played.sentAt.push(win.location.pathname);
    composer.textContent = "";
    goTo(chatPath(win.location.pathname));
    thread.append(...parseHtml(userTurn(prompt) + answerTurn("msg-1", "in_progress", "")));
    later(300, () => {
      thread.querySelectorAll("[data-markdown-copy]").at(-1)!.append(...parseHtml('<p data-d-text-flow="">Four.</p>'));
      thread.querySelectorAll("[data-talvt-turn-state]").at(-1)!.setAttribute("data-talvt-turn-state", "complete");
    });
  };
  return played;
}

// A chat started on a project's page gets `/g/<id>-<slug>/c/<chat id>`; one
// on `/` gets `/c/<chat id>`.
const chatFrom = (from: string): string => {
  const id = projectPageId(from);
  return id === null ? "/c/plain-1" : `/g/${id}-${NAME}/c/chat-1`;
};

// A row click opens its project's page after `ms`.
function rowsNavigate(ms = 100): void {
  for (const r of doc.querySelectorAll("[data-app-action-sidebar-project-row]")) {
    // The row and its expand button only expand; the new-chat button opens
    // the project page.
    r.onClick = () => r.setAttribute("data-app-action-sidebar-project-collapsed", "false");
    const expand = r.querySelector("[data-test-expand]");
    if (expand !== null) {
      expand.onClick = () => r.setAttribute("data-app-action-sidebar-project-collapsed", "false");
    }
    const newChat = r.querySelector("[data-test-new-chat]");
    if (newChat !== null) {
      newChat.onClick = () => later(ms, () => goTo(`/g/${r.getAttribute("data-app-action-sidebar-project-id")}/project`));
    }
  }
}

// The create button opens the dialog; its submit creates project `id` named
// after what was typed and opens its page. Returns what the dialog saw.
function createOpensDialog(id: string): { typed: string[]; escapes: number } {
  const seen = { typed: [] as string[], escapes: 0 };
  el("[data-app-action-sidebar-project-create]").onClick = () => {
    const [dialog] = parseHtml(
      `<div role="dialog"><form><input id="chatgpt-project-name" name="project-name">` +
        `<button type="submit">Создать проект</button></form></div>`,
    ) as FakeElement[];
    doc.body.append(dialog!);
    dialog!.addEventListener("keydown", (ev) => {
      if ((ev as KeyboardEvent).key === "Escape") {
        seen.escapes++;
        dialog!.remove();
      }
    });
    dialog!.querySelector('button[type="submit"]')!.onClick = () => {
      const typed = String((dialog!.querySelector("input#chatgpt-project-name") as unknown as { value?: string }).value ?? "");
      seen.typed.push(typed);
      later(100, () => {
        dialog!.remove();
        el("[data-project-rows]").append(...parseHtml(row(typed, id)));
        goTo(`/g/${id}/project`);
      });
    };
  };
  return seen;
}

// ─── paths ────────────────────────────────────────────────────────────────

test("projects.paths_of_a_project_chat_are_checked_as_strictly_as_a_plain_one", () => {
  const chat = `/g/${PROJ_ID}-${NAME}/c/chat-1`;
  assert.equal(conversationPathFrom(chat), chat);
  assert.equal(conversationPathFrom(`/g/${PROJ_ID}-${NAME}/c/local-chatgpt:1`), null);
  assert.equal(conversationPathFrom(`/g/${PROJ_ID}/project`), null);
  assert.equal(conversationPathFrom(`/g/g-p-1-proj/c/chat-1`), null, "a project id is 32 hex");
  assert.equal(projectPageId(`/g/${PROJ_ID}/project`), PROJ_ID);
  assert.equal(projectPageId(`/g/${PROJ_ID}/project/x`), null);

  const conv = { kind: "conversation" as const, url: `https://chatgpt.com${chat}` };
  assert.equal(targetMatchesPath(conv, chat), true);
  // The same chat after the project was renamed (another slug).
  assert.equal(targetMatchesPath(conv, `/g/${PROJ_ID}-renamed/c/chat-1`), true);
  assert.equal(targetMatchesPath(conv, `/g/${PROJ_ID}-${NAME}/c/chat-2`), false);
  assert.equal(targetMatchesPath(conv, `/g/${OTHER_ID}-${NAME}/c/chat-1`), false, "only this project");
  assert.equal(targetMatchesPath(conv, "/c/chat-1"), false, "only this project");
  assert.equal(targetMatchesPath({ kind: "conversation", url: "https://chatgpt.com/c/chat-1" }, chat), false);

  const inProject = { kind: "new_chat" as const, projectId: PROJ_ID };
  assert.equal(targetMatchesPath(inProject, `/g/${PROJ_ID}/project`), true);
  assert.equal(targetMatchesPath(inProject, `/g/${OTHER_ID}/project`), false);
  assert.equal(targetMatchesPath(inProject, "/"), false);
  assert.equal(targetMatchesPath({ kind: "new_chat" }, `/g/${PROJ_ID}/project`), false);
});

// ─── gateway: a new chat ──────────────────────────────────────────────────

test("projects.gateway_new_chat_goes_into_the_existing_project", async () => {
  try {
    const played = showPage("/", { rows: row("other", OTHER_ID) + row(NAME, PROJ_ID) }, chatFrom);
    rowsNavigate();
    const reply = await ask("g1", PROMPT, { target: { kind: "new_chat" }, project: NAME });
    assert.deepEqual(reply, {
      id: "g1",
      ok: true,
      kind: "test.ask",
      text: "Four.",
      conversationUrl: `https://chatgpt.com/g/${PROJ_ID}-${NAME}/c/chat-1`,
    });
    assert.deepEqual(played.sent, [PROMPT]);
    assert.deepEqual(played.sentAt, [`/g/${PROJ_ID}/project`]);
    assert.equal(doc.querySelector('[data-app-action-sidebar-project-label="other"]')!.clicks, 0);
    assert.equal(el("[data-app-action-sidebar-project-create]").clicks, 0, "nothing is created");
    assert.deepEqual(remembered(), { [NAME]: PROJ_ID }, "found by the sidebar, remembered");
  } finally {
    await settlePage();
  }
});

test("projects.gateway_new_chat_creates_the_missing_project", async () => {
  try {
    const played = showPage("/", { rows: row("other", OTHER_ID) }, chatFrom);
    rowsNavigate();
    const dialog = createOpensDialog(NEW_ID);
    const reply = await ask("g2", PROMPT, { target: { kind: "new_chat" }, project: NAME });
    assert.equal(reply.ok, true, JSON.stringify(reply));
    assert.equal(reply.conversationUrl, `https://chatgpt.com/g/${NEW_ID}-${NAME}/c/chat-1`);
    assert.equal("warning" in reply, false);
    assert.deepEqual(dialog.typed, [NAME]);
    assert.deepEqual(played.sentAt, [`/g/${NEW_ID}/project`]);
    assert.deepEqual(remembered(), { [NAME]: NEW_ID });
  } finally {
    await settlePage();
  }
});

test("projects.gateway_loads_the_project_page_when_the_row_does_not_open_it", async () => {
  try {
    const played = showPage(`/c/old-chat`, { rows: row(NAME, PROJ_ID) }, chatFrom, userTurn("q") + answerTurn("m0", "complete", "a"));
    // The row does nothing: the page asks the background for a page load.
    const first = await ask("g3", PROMPT, { target: { kind: "new_chat" }, project: NAME });
    assert.deepEqual(first, {
      id: "g3",
      ok: true,
      kind: "navigate",
      url: `https://chatgpt.com/g/${PROJ_ID}/project`,
    });
    assert.deepEqual(played.sent, []);
    // The background loaded it and asks again; the sidebar has not shown
    // the project's row yet, and nothing is created for it.
    const again = showPage(`/g/${PROJ_ID}/project`, {}, chatFrom);
    const dialog = createOpensDialog(NEW_ID);
    const reply = await ask("g3", PROMPT, { target: { kind: "new_chat" }, project: NAME, reloaded: true });
    assert.equal(reply.conversationUrl, `https://chatgpt.com/g/${PROJ_ID}-${NAME}/c/chat-1`, JSON.stringify(reply));
    assert.deepEqual(again.sentAt, [`/g/${PROJ_ID}/project`]);
    assert.deepEqual(dialog.typed, []);
  } finally {
    await settlePage();
  }
});

// Ruling 51: no project can be found or made (here: drifted markup, no rows
// and no create button) — the chat goes to a plain new chat with a warning.
test("projects.gateway_falls_back_to_a_plain_new_chat_with_a_warning", async () => {
  try {
    const played = showPage("/", { create: false }, chatFrom);
    const reply = await ask("g4", PROMPT, { target: { kind: "new_chat" }, project: NAME });
    assert.equal(reply.ok, true, JSON.stringify(reply));
    assert.equal(reply.conversationUrl, "https://chatgpt.com/c/plain-1");
    assert.match(String((reply as { warning?: string }).warning), /ChatGPT project "my-proj" was not opened or created/);
    assert.deepEqual(played.sentAt, ["/"]);
  } finally {
    await settlePage();
  }
});

// Live check 2026-10-28: a closed header menu around the create button was
// taken for a collapsed list, so the project was neither found nor created.
// Live check 2026-10-28: with eight projects only the five newest were listed;
// an older one behind "show more" would have been created a second time.
test("projects.a_project_behind_show_more_is_found_not_created_again", async () => {
  try {
    const newest = [1, 2, 3, 4, 5].map((n) => row(`newer-${n}`, `g-p-${String(n).repeat(32)}`)).join("");
    // An expanded project nests its chats, each with buttons of its own.
    const expandedChats =
      `<div role="list"><div role="listitem"><a href="/g/x/c/1">chat</a>` +
      `<button type="button" aria-label="Действия чата" data-test-chat-actions=""></button>` +
      `<button type="button" aria-label="Закрепить чат"></button></div></div>`;
    const played = showPage("/", { rows: newest + expandedChats, more: true }, chatFrom);
    const more = el("[data-test-show-more]");
    more.onClick = () =>
      later(50, () => {
        el("[data-project-rows]").append(...parseHtml(row(NAME, PROJ_ID)));
        more.remove();
        rowsNavigate();
      });
    rowsNavigate();
    const dialog = createOpensDialog(NEW_ID);
    const reply = await ask("g9", PROMPT, { target: { kind: "new_chat" }, project: NAME });
    assert.equal(reply.conversationUrl, `https://chatgpt.com/g/${PROJ_ID}-${NAME}/c/chat-1`, JSON.stringify(reply));
    assert.equal((reply as { warning?: string }).warning, undefined, JSON.stringify(reply));
    assert.deepEqual(dialog.typed, [], "no second project is created");
    assert.equal(more.clicks, 1, "the list was asked to show the rest");
    assert.equal(el("[data-test-chat-actions]").clicks, 0, "a chat's own button is not pressed");
    assert.deepEqual(played.sentAt, [`/g/${PROJ_ID}/project`]);
  } finally {
    await settlePage();
  }
});

test("projects.a_closed_header_menu_does_not_stop_the_project_being_found", async () => {
  try {
    const played = showPage("/", { rows: row(NAME, PROJ_ID) }, chatFrom);
    rowsNavigate();
    const dialog = createOpensDialog(NEW_ID);
    const reply = await ask("g5", PROMPT, { target: { kind: "new_chat" }, project: NAME });
    assert.equal(reply.conversationUrl, `https://chatgpt.com/g/${PROJ_ID}-${NAME}/c/chat-1`, JSON.stringify(reply));
    assert.equal((reply as { warning?: string }).warning, undefined, JSON.stringify(reply));
    assert.deepEqual(dialog.typed, []);
    assert.equal(el("[data-header-menu]").clicks, 0, "the header menu is not clicked");
    assert.deepEqual(played.sentAt, [`/g/${PROJ_ID}/project`]);
  } finally {
    await settlePage();
  }
});

test("projects.a_closed_header_menu_does_not_stop_the_project_being_created", async () => {
  try {
    const played = showPage("/", {}, chatFrom);
    const dialog = createOpensDialog(NEW_ID);
    const reply = await ask("g6", PROMPT, { target: { kind: "new_chat" }, project: NAME });
    assert.equal(reply.ok, true, JSON.stringify(reply));
    assert.equal((reply as { warning?: string }).warning, undefined, JSON.stringify(reply));
    assert.deepEqual(dialog.typed, [NAME], "the project is created");
    assert.equal(el("[data-header-menu]").clicks, 0, "the header menu is not clicked");
    assert.deepEqual(played.sentAt, [`/g/${NEW_ID}/project`]);
  } finally {
    await settlePage();
  }
});

test("projects.a_create_dialog_without_its_field_is_closed_and_the_chat_is_plain", async () => {
  try {
    const played = showPage("/", {}, chatFrom);
    let escapes = 0;
    el("[data-app-action-sidebar-project-create]").onClick = () => {
      // A drifted dialog: the name field is not where it was.
      const [dialog] = parseHtml(`<div role="dialog"><input id="chatgpt-project-name"></div>`) as FakeElement[];
      doc.body.append(dialog!);
      dialog!.addEventListener("keydown", (ev) => {
        if ((ev as KeyboardEvent).key === "Escape") {
          escapes++;
          dialog!.remove();
        }
      });
    };
    const reply = await ask("g7", PROMPT, { target: { kind: "new_chat" }, project: NAME });
    assert.equal(reply.ok, true, JSON.stringify(reply));
    assert.match(String((reply as { warning?: string }).warning), /no name field or submit button/);
    assert.equal(escapes, 1, "the dialog was closed");
    assert.deepEqual(played.sentAt, ["/"]);
  } finally {
    await settlePage();
  }
});

// The answer's address must be in the project the chat was opened in.
test("projects.an_answer_from_another_project_is_not_taken", async () => {
  try {
    showPage("/", { rows: row(NAME, PROJ_ID) }, () => `/g/${OTHER_ID}-x/c/chat-9`);
    rowsNavigate();
    const reply = await ask("g8", PROMPT, { target: { kind: "new_chat" }, project: NAME });
    assert.equal(reply.ok, false, JSON.stringify(reply));
    assert.ok(
      reply.errorCode === "chat_url_missing" || reply.errorCode === "target_mismatch",
      JSON.stringify(reply),
    );
  } finally {
    await settlePage();
  }
});

// ─── gateway: a chat that exists ──────────────────────────────────────────

test("projects.gateway_continues_a_project_chat_and_ignores_the_project", async () => {
  try {
    const chat = `/g/${PROJ_ID}-${NAME}/c/chat-1`;
    const played = showPage(
      chat,
      { rows: row(NAME, PROJ_ID) },
      () => chat,
      userTurn("q") + answerTurn("m0", "complete", "a"),
    );
    rowsNavigate();
    const reply = await ask("g9", PROMPT, {
      target: { kind: "conversation", url: `https://chatgpt.com${chat}` },
      project: "another",
    });
    assert.equal(reply.ok, true, JSON.stringify(reply));
    assert.equal(reply.conversationUrl, `https://chatgpt.com${chat}`);
    assert.deepEqual(played.sentAt, [chat]);
    assert.equal(el("[data-app-action-sidebar-project-create]").clicks, 0);
  } finally {
    await settlePage();
  }
});

// ─── CLI: the user's own tab ──────────────────────────────────────────────

test("projects.cli_new_chat_goes_into_the_project", async () => {
  try {
    const played = showPage("/", { rows: row(NAME, PROJ_ID) }, chatFrom);
    rowsNavigate();
    const reply = await ask("c1", PROMPT, { project: NAME });
    assert.equal(reply.ok, true, JSON.stringify(reply));
    assert.equal(reply.text, "Four.");
    assert.deepEqual(played.sentAt, [`/g/${PROJ_ID}/project`]);
    assert.equal(reply.conversationUrl, `https://chatgpt.com/g/${PROJ_ID}-${NAME}/c/chat-1`);
  } finally {
    await settlePage();
  }
});

test("projects.cli_ask_into_a_chat_continues_it_where_it_is", async () => {
  try {
    const played = showPage("/c/user-chat", { rows: row(NAME, PROJ_ID) }, () => "/c/user-chat", userTurn("q") + answerTurn("m0", "complete", "a"));
    rowsNavigate();
    const reply = await ask("c2", PROMPT, { project: NAME });
    assert.equal(reply.ok, true, JSON.stringify(reply));
    assert.deepEqual(played.sentAt, ["/c/user-chat"]);
    assert.equal(doc.querySelector("[data-app-action-sidebar-project-row]")!.clicks, 0);
  } finally {
    await settlePage();
  }
});

test("projects.cli_new_chat_without_a_project_stays_plain_with_a_warning", async () => {
  try {
    const played = showPage("/", { create: false }, chatFrom);
    const reply = await ask("c3", PROMPT, { project: NAME });
    assert.equal(reply.ok, true, JSON.stringify(reply));
    assert.match(String((reply as { warning?: string }).warning), /The new chat was not put into ChatGPT project "my-proj"/);
    assert.deepEqual(played.sentAt, ["/"]);
  } finally {
    await settlePage();
  }
});

test("projects.a_bad_project_name_is_refused", async () => {
  try {
    showPage("/", { rows: row(NAME, PROJ_ID) }, chatFrom);
    for (const project of ["", " x", "a\u0007b", "я".repeat(101), 7]) {
      const reply = await ask("b1", PROMPT, { target: { kind: "new_chat" }, project });
      assert.equal(reply.errorCode, "bad_request", JSON.stringify(project));
    }
  } finally {
    await settlePage();
  }
});

// ─── I1: rows render after the create button ──────────────────────────────

test("projects.a_row_that_renders_after_the_create_button_is_found_and_nothing_is_created", async () => {
  try {
    const played = showPage("/", {}, chatFrom);
    const dialog = createOpensDialog(NEW_ID);
    // The account's projects come 2 s after the static sidebar.
    later(2_000, () => {
      el("[data-project-rows]").append(...parseHtml(row("other", OTHER_ID) + row(NAME, PROJ_ID)));
      rowsNavigate();
    });
    const reply = await ask("r1", PROMPT, { target: { kind: "new_chat" }, project: NAME });
    assert.equal(reply.conversationUrl, `https://chatgpt.com/g/${PROJ_ID}-${NAME}/c/chat-1`, JSON.stringify(reply));
    assert.deepEqual(dialog.typed, [], "no duplicate is created");
    assert.deepEqual(played.sentAt, [`/g/${PROJ_ID}/project`]);
  } finally {
    await settlePage();
  }
});

// ─── the remembered project ids (Ruling 52) ───────────────────────────────

test("projects.gateway_loads_the_remembered_project_when_the_sidebar_does_not_show_it", async () => {
  try {
    remember({ [NAME]: PROJ_ID });
    // The sidebar shows no project rows (hidden behind "more", say).
    showPage("/", {}, chatFrom);
    const dialog = createOpensDialog(NEW_ID);
    const first = await ask("m1", PROMPT, { target: { kind: "new_chat" }, project: NAME });
    assert.deepEqual(first, { id: "m1", ok: true, kind: "navigate", url: `https://chatgpt.com/g/${PROJ_ID}/project` });
    const again = showPage(`/g/${PROJ_ID}/project`, {}, chatFrom);
    createOpensDialog(NEW_ID);
    const reply = await ask("m1", PROMPT, { target: { kind: "new_chat" }, project: NAME, reloaded: true });
    assert.equal(reply.conversationUrl, `https://chatgpt.com/g/${PROJ_ID}-${NAME}/c/chat-1`, JSON.stringify(reply));
    assert.deepEqual(again.sentAt, [`/g/${PROJ_ID}/project`]);
    assert.deepEqual(dialog.typed, []);
    assert.deepEqual(remembered(), { [NAME]: PROJ_ID });
  } finally {
    await settlePage();
  }
});

test("projects.a_remembered_page_that_does_not_hold_is_forgotten_and_the_sidebar_searched", async () => {
  try {
    remember({ [NAME]: OTHER_ID });
    showPage("/", { rows: row(NAME, PROJ_ID) }, chatFrom);
    const first = await ask("m2", PROMPT, { target: { kind: "new_chat" }, project: NAME });
    assert.equal(first.kind, "navigate");
    assert.equal((first as { url?: string }).url, `https://chatgpt.com/g/${OTHER_ID}/project`);
    // The load of the dead id ends on the start page (another account, a
    // deleted project).
    const again = showPage("/", { rows: row(NAME, PROJ_ID) }, chatFrom);
    rowsNavigate();
    const reply = await ask("m2", PROMPT, { target: { kind: "new_chat" }, project: NAME, reloaded: true });
    assert.equal(reply.conversationUrl, `https://chatgpt.com/g/${PROJ_ID}-${NAME}/c/chat-1`, JSON.stringify(reply));
    assert.deepEqual(again.sentAt, [`/g/${PROJ_ID}/project`]);
    assert.deepEqual(remembered(), { [NAME]: PROJ_ID });
  } finally {
    await settlePage();
  }
});

test("projects.a_remembered_id_whose_row_shows_another_name_is_forgotten", async () => {
  try {
    remember({ [NAME]: OTHER_ID, keep: PROJ_ID });
    const played = showPage("/", { rows: row("renamed", OTHER_ID) + row(NAME, PROJ_ID) }, chatFrom);
    rowsNavigate();
    // A CLI ask: the remembered id only finds the row.
    const reply = await ask("m3", PROMPT, { project: NAME });
    assert.equal(reply.ok, true, JSON.stringify(reply));
    assert.deepEqual(played.sentAt, [`/g/${PROJ_ID}/project`]);
    assert.equal(doc.querySelector('[data-app-action-sidebar-project-label="renamed"]')!.clicks, 0);
    assert.deepEqual(remembered(), { [NAME]: PROJ_ID, keep: PROJ_ID });
  } finally {
    await settlePage();
  }
});

test("projects.cli_finds_the_remembered_project_by_its_row", async () => {
  try {
    remember({ [NAME]: PROJ_ID });
    const played = showPage("/", { rows: row(NAME, PROJ_ID) }, chatFrom);
    rowsNavigate();
    const reply = await ask("m4", PROMPT, { project: NAME });
    assert.equal(reply.ok, true, JSON.stringify(reply));
    assert.deepEqual(played.sentAt, [`/g/${PROJ_ID}/project`]);
  } finally {
    await settlePage();
  }
});

test("projects.a_project_whose_chat_gets_no_address_is_forgotten", async () => {
  try {
    remember({ [NAME]: PROJ_ID });
    // Already on the remembered project's page; its chats never get saved.
    showPage(`/g/${PROJ_ID}/project`, {}, () => `/g/${PROJ_ID}-${NAME}/c/local-chatgpt:1`);
    const reply = await ask("m5", PROMPT, { target: { kind: "new_chat" }, project: NAME });
    assert.equal(reply.errorCode, "chat_url_missing", JSON.stringify(reply));
    assert.deepEqual(remembered(), {});
  } finally {
    await settlePage();
  }
});

test("projects.an_nfd_folder_name_finds_the_nfc_project", async () => {
  try {
    const nfc = "мой проект";
    const nfd = nfc.normalize("NFD");
    assert.notEqual(nfd, nfc, "the test needs a name NFD changes");
    const played = showPage("/", { rows: row(nfc, PROJ_ID) }, chatFrom);
    rowsNavigate();
    const dialog = createOpensDialog(NEW_ID);
    const reply = await ask("u1", PROMPT, { target: { kind: "new_chat" }, project: nfd });
    assert.equal(reply.ok, true, JSON.stringify(reply));
    assert.deepEqual(played.sentAt, [`/g/${PROJ_ID}/project`]);
    assert.deepEqual(dialog.typed, []);
    // Remembered under the NFC name: the NFD one finds it too.
    assert.deepEqual(remembered(), { [nfc]: PROJ_ID });
  } finally {
    await settlePage();
  }
});
