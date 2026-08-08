// TanStack Query ownership for the administration API (issue #163). Admin fetchers remain the
// transport boundary in `admin.ts`; this module gives every settings section the same cache keys,
// independent loading/error state, and mutation invalidation rules.

import { useMutation, useQuery, useQueryClient, type QueryClient, type QueryKey } from "@tanstack/react-query";
import {
  admin,
  type CacheTarget,
  type FlagKey,
  type NewAccount,
  type NewToken,
  type SetFlag,
  type UpdateAccount,
} from "./admin";
import { qk } from "./queries";
import type { LinkOidcIdentity, SetOidcConfig } from "./types";

interface AdminQueryOptions {
  enabled?: boolean;
}

const overviewKeys: QueryKey[] = [qk.adminStatus, qk.adminFlags, qk.adminTokens, qk.adminAudit];
const directoryKeys: QueryKey[] = [qk.adminAccounts, qk.adminGroups];

/** Refetch access first. If it has just become unauthorized, dependent admin queries become
 * inactive before their invalidations run, preserving the access gate's no-parallel-403 rule. */
async function invalidateOverview(client: QueryClient): Promise<void> {
  await client.invalidateQueries({ queryKey: qk.adminStatus });
  await Promise.all(
    overviewKeys.slice(1).map((queryKey) => client.invalidateQueries({ queryKey })),
  );
}

async function invalidateKeys(client: QueryClient, keys: QueryKey[]): Promise<void> {
  await Promise.all(keys.map((queryKey) => client.invalidateQueries({ queryKey })));
}

function useInvalidatingAdminMutation<TData, TVariables>(
  mutationFn: (variables: TVariables) => Promise<TData>,
  invalidation: (client: QueryClient) => Promise<void>,
) {
  const client = useQueryClient();
  // Settings owns several errors as UI flow rather than failure notifications: exposure-required
  // is a confirmation prompt, and row conflicts remain visible inside their section. Capture the
  // transport rejection as mutation data so the app-wide MutationCache does not raise a duplicate
  // toast (or an error toast before the exposure dialog), then rethrow from this hook's public
  // `mutateAsync` wrapper for the existing local handlers.
  const mutation = useMutation({
    mutationFn: async (variables: TVariables) => {
      try {
        return { ok: true as const, data: await mutationFn(variables) };
      } catch (error) {
        return { ok: false as const, error };
      }
    },
    onSuccess: (result) => (result.ok ? invalidation(client) : undefined),
  });
  return {
    ...mutation,
    mutateAsync: async (variables: TVariables): Promise<TData> => {
      const result = await mutation.mutateAsync(variables);
      if (!result.ok) throw result.error;
      return result.data;
    },
  };
}

export function useAdminStatus() {
  return useQuery({ queryKey: qk.adminStatus, queryFn: admin.status, retry: false });
}

export function useAdminFlags(options: AdminQueryOptions = {}) {
  return useQuery({
    queryKey: qk.adminFlags,
    queryFn: admin.flags,
    enabled: options.enabled,
    retry: false,
  });
}

export function useAdminTokens(options: AdminQueryOptions = {}) {
  return useQuery({
    queryKey: qk.adminTokens,
    queryFn: admin.tokens,
    enabled: options.enabled,
    retry: false,
  });
}

export function useAdminAudit(limit = 25, options: AdminQueryOptions = {}) {
  return useQuery({
    queryKey: [...qk.adminAudit, limit],
    queryFn: () => admin.audit(limit),
    enabled: options.enabled,
    retry: false,
  });
}

export function useAdminStorageUsage(options: AdminQueryOptions = {}) {
  return useQuery({
    queryKey: qk.adminStorage,
    queryFn: admin.storageUsage,
    enabled: options.enabled,
    retry: false,
  });
}

export function useAdminAccounts(options: AdminQueryOptions = {}) {
  return useQuery({
    queryKey: qk.adminAccounts,
    queryFn: admin.accounts,
    enabled: options.enabled,
    retry: false,
  });
}

export function useAdminGroups(options: AdminQueryOptions = {}) {
  return useQuery({
    queryKey: qk.adminGroups,
    queryFn: admin.groups,
    enabled: options.enabled,
    retry: false,
  });
}

export function useAdminOidcConfig(options: AdminQueryOptions = {}) {
  return useQuery({
    queryKey: qk.adminOidcConfig,
    queryFn: admin.oidcConfig,
    enabled: options.enabled,
    retry: false,
  });
}

export function useAdminOidcIdentities(options: AdminQueryOptions = {}) {
  return useQuery({
    queryKey: qk.adminOidcIdentities,
    queryFn: admin.oidcIdentities,
    enabled: options.enabled,
    retry: false,
  });
}

export function useSetAdminFlag() {
  return useInvalidatingAdminMutation(
    ({ key, request }: { key: FlagKey; request: SetFlag }) => admin.setFlag(key, request),
    invalidateOverview,
  );
}

export function useCreateAdminToken() {
  return useInvalidatingAdminMutation((request: NewToken) => admin.createToken(request), invalidateOverview);
}

export function useRevokeAdminToken() {
  return useInvalidatingAdminMutation((id: string) => admin.revokeToken(id), invalidateOverview);
}

export function useCreateAdminAccount() {
  return useInvalidatingAdminMutation(
    (request: NewAccount) => admin.createAccount(request),
    (client) => invalidateKeys(client, directoryKeys),
  );
}

export function useUpdateAdminAccount() {
  return useInvalidatingAdminMutation(
    ({ id, request }: { id: string; request: UpdateAccount }) => admin.updateAccount(id, request),
    (client) => invalidateKeys(client, directoryKeys),
  );
}

export function useDeleteAdminAccount() {
  return useInvalidatingAdminMutation(
    (id: string) => admin.deleteAccount(id),
    (client) => invalidateKeys(client, directoryKeys),
  );
}

export function useRevokeAdminAccountSessions() {
  return useInvalidatingAdminMutation(
    (id: string) => admin.revokeAccountSessions(id),
    (client) => invalidateKeys(client, directoryKeys),
  );
}

export function useCreateAdminGroup() {
  return useInvalidatingAdminMutation(
    (name: string) => admin.createGroup(name),
    (client) => invalidateKeys(client, directoryKeys),
  );
}

export function useDeleteAdminGroup() {
  return useInvalidatingAdminMutation(
    (id: string) => admin.deleteGroup(id),
    (client) => invalidateKeys(client, directoryKeys),
  );
}

export function useSetAdminGroupMembers() {
  return useInvalidatingAdminMutation(
    ({ id, accountIds }: { id: string; accountIds: string[] }) =>
      admin.setGroupMembers(id, accountIds),
    (client) => invalidateKeys(client, directoryKeys),
  );
}

export function useSetAdminOidcConfig() {
  return useInvalidatingAdminMutation(
    (request: SetOidcConfig) => admin.setOidcConfig(request),
    (client) => invalidateKeys(client, [qk.adminOidcConfig, qk.adminOidcIdentities]),
  );
}

export function useLinkAdminOidcIdentity() {
  return useInvalidatingAdminMutation(
    (request: LinkOidcIdentity) => admin.linkOidcIdentity(request),
    (client) => invalidateKeys(client, [qk.adminOidcIdentities]),
  );
}

export function useUnlinkAdminOidcIdentity() {
  return useInvalidatingAdminMutation(
    ({ subject, issuer }: { subject: string; issuer?: string }) =>
      admin.unlinkOidcIdentity(subject, issuer),
    (client) => invalidateKeys(client, [qk.adminOidcIdentities]),
  );
}

async function invalidateMaintenance(client: QueryClient): Promise<void> {
  await invalidateOverview(client);
  await client.invalidateQueries({ queryKey: qk.adminStorage });
}

export function useClearAdminCache() {
  return useInvalidatingAdminMutation((target: CacheTarget) => admin.clearCache(target), invalidateMaintenance);
}

export function useClearAdminAnalysis() {
  return useInvalidatingAdminMutation(() => admin.clearAnalysis(), invalidateMaintenance);
}

export function useVacuumAdminStorage() {
  return useInvalidatingAdminMutation(() => admin.vacuum(), invalidateMaintenance);
}

export function useWipeAdminCatalog() {
  return useInvalidatingAdminMutation((confirm: boolean) => admin.wipe(confirm), invalidateMaintenance);
}

export function useFactoryResetAdmin() {
  return useInvalidatingAdminMutation(
    (confirm: boolean) => admin.factoryReset(confirm),
    invalidateMaintenance,
  );
}
