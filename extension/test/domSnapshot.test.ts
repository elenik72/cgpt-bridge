// Self-test of the snapshot harness (test/harness/domSnapshot.ts): its
// selector evaluator must refuse what it does not support, wherever that sits
// in the selector.

import { test } from "node:test";
import assert from "node:assert/strict";

import { parseHtml } from "./harness/domSnapshot";
import type { FakeElement } from "./harness/fakeWindow";

const el = (html: string): FakeElement => parseHtml(html)[0] as FakeElement;

test("domSnapshot.supported_selectors_match", () => {
  const e = el('<div class="a b" data-x="one two"><p>t</p></div>');
  assert.equal(e.matches(".a"), true);
  assert.equal(e.matches('[data-x~="two"], .zzz'), true);
  assert.equal(e.matches("div:has(p)"), true);
  assert.equal(e.matches("div > p"), false);
  assert.equal(e.matches(".zzz"), false);
});

test("domSnapshot.unsupported_selector_in_a_later_branch_throws_even_when_an_earlier_one_matches", () => {
  const e = el('<div class="a"><p>t</p></div>');
  // `.a` matches, so a lazy `some` would never read the second branch.
  assert.equal(e.matches(".a"), true);
  assert.throws(() => e.matches(".a, :first-child"), /selector not supported/);
  assert.throws(() => e.matches(".a, div:not(.b)"), /selector not supported/);
  assert.throws(() => e.querySelectorAll(".a, [data-x|=en]"), /selector not supported/);
  // Also inside `:has(...)` and in a combinator chain.
  assert.throws(() => e.matches(".a, div:has(p, :last-child)"), /selector not supported/);
  assert.throws(() => e.matches(".a, div > > p"), /selector not supported/);
});
