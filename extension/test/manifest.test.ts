// The manifest asks for what docs/security.md §11 lists and no more, and
// declares its extension-page CSP itself rather than relying on Chrome's
// MV3 default.

import { test } from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";

const manifest = JSON.parse(readFileSync(new URL("../manifest.json", import.meta.url), "utf8"));

test("manifest.permissions_are_the_documented_ones", () => {
  assert.deepEqual(manifest.permissions, ["nativeMessaging", "storage", "scripting"]);
  assert.deepEqual(manifest.host_permissions, ["https://chatgpt.com/*"]);
  assert.equal(manifest.web_accessible_resources, undefined);
  assert.equal(manifest.externally_connectable, undefined);
});

test("manifest.declares_the_extension_pages_csp", () => {
  assert.deepEqual(manifest.content_security_policy, {
    extension_pages: "script-src 'self'; object-src 'self'",
  });
});
