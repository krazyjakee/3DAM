/** Stable paths for the two credential surfaces. Keeping them here prevents navigation and
 * route registration from drifting back into a single, ambiguous "Settings" destination. */
export const ACCOUNT_ROUTES = {
  profile: "/profile",
  administration: "/admin",
  legacySettings: "/settings",
} as const;

export type ProfileAccessState = "account" | "sign-in" | "unavailable";

/** Resolve a direct Profile visit from server capability + current identity. */
export function profileAccessState(
  accountsEnabled: boolean,
  hasPersonalAccount: boolean,
): ProfileAccessState {
  if (!accountsEnabled) return "unavailable";
  return hasPersonalAccount ? "account" : "sign-in";
}

/** Navigation exposes only surfaces the current identity can actually use. */
export function accountNavigation(
  hasPersonalAccount: boolean,
  canAdminister: boolean,
): { profile: boolean; administration: boolean } {
  return {
    profile: hasPersonalAccount,
    administration: canAdminister,
  };
}
