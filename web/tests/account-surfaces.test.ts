import assert from "node:assert/strict";
import test from "node:test";

import {
  ACCOUNT_ROUTES,
  accountNavigation,
  profileAccessState,
} from "../src/lib/account-surfaces.ts";

test("signed-in editors and viewers get Profile without an Administration advertisement", () => {
  assert.deepEqual(accountNavigation(true, false), {
    profile: true,
    administration: false,
  });
});

test("an admin capability exposes Administration independently of a personal account", () => {
  assert.deepEqual(accountNavigation(false, true), {
    profile: false,
    administration: true,
  });
  assert.deepEqual(accountNavigation(true, true), {
    profile: true,
    administration: true,
  });
});

test("Profile deep links distinguish account, sign-in, and accounts-disabled states", () => {
  assert.equal(profileAccessState(true, true), "account");
  assert.equal(profileAccessState(true, false), "sign-in");
  assert.equal(profileAccessState(false, false), "unavailable");
});

test("account routes keep personal and administrative destinations separate", () => {
  assert.equal(ACCOUNT_ROUTES.profile, "/profile");
  assert.equal(ACCOUNT_ROUTES.administration, "/admin");
  assert.equal(ACCOUNT_ROUTES.legacySettings, "/settings");
});
