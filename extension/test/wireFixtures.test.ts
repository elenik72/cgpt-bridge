// The frames the background sends to the native host, checked against the
// fixtures the Rust protocol crate parses (protocol/tests/fixtures/). A field
// renamed on either side fails here or in protocol/tests/wire_fixtures.rs
// instead of timing out a live request.

import { test } from "node:test";
import assert from "node:assert/strict";
import { readFileSync, readdirSync } from "node:fs";

import { parseHostTarget } from "../src/askControl";
import {
  EXTENSION_BUILD,
  WIRE_ERROR_CODES,
  askResultFrame,
  errorFrame,
  forOlderHost,
  hostBuildOf,
  isValidProjectName,
  MAX_PROJECT_NAME_CHARS,
  statusResultFrame,
  stoppedFrame,
  type WireErrorCode,
} from "../src/bridgeProtocol";

// The test file and its bundle (.test-build/) both sit one level below
// extension/, so the same relative path works for either.
const FIXTURES = new URL("../../protocol/tests/fixtures/", import.meta.url);

function fixture(name: string): Record<string, unknown> {
  return JSON.parse(readFileSync(new URL(`${name}.json`, FIXTURES), "utf8"));
}

// Frames as components built before builds were reported send them.
function legacy(name: string): Record<string, unknown> {
  return fixture(`legacy/${name}`);
}

function without(frame: Record<string, unknown>, key: string): Record<string, unknown> {
  const copy = { ...frame };
  delete copy[key];
  return copy;
}

test("wire.ask_result_matches_fixture", () => {
  const fx = fixture("response_ask_result");
  assert.deepEqual(
    askResultFrame(fx.id as string, fx.text as string, fx.conversation_url as string),
    fx,
  );
  // No saved chat: the field is left out (Rust reads it as None).
  assert.deepEqual(
    askResultFrame(fx.id as string, fx.text as string, null),
    without(fx, "conversation_url"),
  );
});

test("wire.ask_result_with_a_warning_matches_fixture", () => {
  const fx = fixture("response_ask_result_warning");
  assert.deepEqual(
    askResultFrame(fx.id as string, fx.text as string, fx.conversation_url as string, fx.warning as string),
    fx,
  );
});

test("wire.project_names_from_fixtures_are_valid_and_the_rust_rules_hold", () => {
  for (const name of ["request_ask_project", "request_ask_new_chat_project"]) {
    assert.equal(isValidProjectName(fixture(name).project), true, name);
  }
  assert.equal(isValidProjectName("я".repeat(MAX_PROJECT_NAME_CHARS)), true);
  // Code points, as Rust counts chars: an emoji is one.
  assert.equal(isValidProjectName("😀".repeat(MAX_PROJECT_NAME_CHARS)), true);
  for (const bad of ["", "   ", " lead", "trail ", "a\nb", "a\u001b[2Jb", "tab\there", "a\u0085b", "я".repeat(101), 1, null]) {
    assert.equal(isValidProjectName(bad), false, JSON.stringify(bad));
  }
});

test("wire.stopped_matches_fixture", () => {
  const fx = fixture("response_stopped");
  assert.deepEqual(stoppedFrame(fx.id as string, fx.was_generating as boolean), fx);
  const other = fixture("response_stopped_other_chat");
  assert.deepEqual(stoppedFrame(other.id as string, other.was_generating as boolean, true), other);
  const notSent = fixture("response_stopped_not_sent");
  assert.deepEqual(stoppedFrame(notSent.id as string, notSent.was_generating as boolean, false, true), notSent);
});

test("wire.status_result_matches_fixture", () => {
  const fx = fixture("response_status_result");
  const status = {
    tabOpen: fx.tab_open as boolean,
    loggedIn: fx.logged_in as boolean,
    conversationUrl: fx.conversation_url as string,
  };
  const build = fx.build as string;
  assert.deepEqual(statusResultFrame(fx.id as string, status, build), fx);
  // The extension's own build by default.
  assert.deepEqual(statusResultFrame(fx.id as string, status), { ...fx, build: EXTENSION_BUILD });
  // No chat: the field is sent as null, not left out.
  assert.deepEqual(statusResultFrame(fx.id as string, { ...status, conversationUrl: null }, build), {
    ...fx,
    conversation_url: null,
  });
  // Both flags flipped: each one is taken from its own input (the fixture
  // alone has `extension` and `tab_open` both true).
  assert.deepEqual(
    statusResultFrame(
      fx.id as string,
      { ...status, tabOpen: !status.tabOpen, loggedIn: !status.loggedIn },
      build,
    ),
    { ...fx, tab_open: !status.tabOpen, logged_in: !status.loggedIn },
  );
});

test("wire.error_codes_match_fixtures", () => {
  const prefix = "response_error_";
  const names = readdirSync(FIXTURES)
    .filter((f) => f.startsWith(prefix) && f.endsWith(".json"))
    .map((f) => f.slice(0, -".json".length));
  assert.deepEqual(
    names.map((n) => n.slice(prefix.length)).sort(),
    [...WIRE_ERROR_CODES].sort(),
    "the extension's error codes are the protocol's ErrorCode set",
  );
  for (const name of names) {
    const fx = fixture(name);
    assert.deepEqual(
      errorFrame(fx.id as string, fx.code as WireErrorCode, fx.message as string),
      fx,
      name,
    );
  }
});

test("wire.ask_targets_from_fixtures_parse", () => {
  assert.deepEqual(parseHostTarget(fixture("request_ask").target), {
    ok: true,
    target: undefined,
  });
  for (const name of ["request_ask_conversation", "request_ask_new_chat", "request_ask_new_chat_project"]) {
    const fx = fixture(name);
    assert.deepEqual(parseHostTarget(fx.target), { ok: true, target: fx.target }, name);
  }
});

test("wire.status_result_differs_from_the_legacy_one_only_by_build", () => {
  // An older reader ignores the added field; nothing else changed.
  const { build, ...rest } = fixture("response_status_result");
  assert.equal(typeof build, "string");
  assert.deepEqual(rest, legacy("response_status_result"));
});

test("wire.host_build_is_read_from_pong_and_absent_from_an_older_host", () => {
  assert.equal(hostBuildOf(fixture("response_pong")), fixture("response_pong").build);
  assert.equal(hostBuildOf(legacy("response_pong")), null);
  // Only a build id (1-64 of A-Za-z0-9._-) counts; anything else is no build.
  for (const bad of ["", "a b", "x\u001b[2J", "line\nbreak", "a".repeat(65), 42]) {
    assert.equal(hostBuildOf({ build: bad }), null, JSON.stringify(bad));
  }
  assert.equal(hostBuildOf({ build: "v1.2_x-3" }), "v1.2_x-3");
  assert.equal(hostBuildOf({ build: "a".repeat(64) }), "a".repeat(64));
});

test("wire.tab_busy_goes_to_an_older_host_as_dom_failure_with_the_same_message", () => {
  // A host without `tab_busy` would fail the whole frame; before the code a
  // busy tab was a DOM failure, with the same text.
  const fx = fixture("response_error_tab_busy");
  const sent = forOlderHost(errorFrame(fx.id as string, "tab_busy", fx.message as string));
  assert.deepEqual(sent, { ...fx, code: "dom_failure" });
});

test("wire.cancelled_goes_to_an_older_host_as_internal_with_the_same_message", () => {
  // A host without `cancelled` would fail the whole frame; the gateway
  // still recognizes a stop by these exact messages (stop_messages.txt).
  const fx = fixture("response_error_cancelled");
  const sent = forOlderHost(errorFrame(fx.id as string, "cancelled", fx.message as string));
  assert.deepEqual(sent, { ...fx, code: "internal" });
  // Every other frame goes out as it is.
  for (const name of ["response_error_timeout", "response_ask_result", "response_stopped"]) {
    const frame = fixture(name) as never;
    assert.equal(forOlderHost(frame), frame, name);
  }
});
