// TanStack Query hooks over the typed client (tech-spec 09 §B.2). Server state lives here;
// UI state (selection, view toggle, filter chips) stays local/URL-driven. The WebSocket
// (ws.ts) invalidates these keys on live events.

import {
  useInfiniteQuery,
  useMutation,
  useQuery,
  useQueryClient,
} from "@tanstack/react-query";
import { api } from "./client";
import type { Scope } from "./admin";
import { bustThumbnails } from "@/lib/thumbnail-cache";
import { getServer, hasBearerCredential } from "@/lib/server";
import {
  BROWSE_MAX_PAGES,
  BROWSE_PAGE_SIZE,
  browseCursorDirectory,
  type BrowsePageParam,
} from "@/lib/browse-window";
import type {
  AddSource,
  AnalyzeRequest,
  Asset,
  AssetId,
  CollectionId,
  CollectionMembers,
  ContentHash,
  ConvertRequest,
  DupRequest,
  DupReviewRequest,
  ExportRequest,
  FavoriteRequest,
  JobId,
  JobListRequest,
  NewCollection,
  QueryRequest,
  RemoveAsset,
  ScanRequest,
  SourceId,
  SuggestionReview,
  TagEditRequest,
  UpdateCollection,
} from "./types";

/** Stable query-key roots — ws.ts invalidates against these. */
export const qk = {
  version: ["version"] as const,
  whoami: ["whoami"] as const,
  assets: ["assets"] as const,
  // Remote ids are only unique within their owning peer. Keep the owner in every detail cache key.
  asset: (id: AssetId, source?: SourceId | null) => ["asset", source ?? "local-or-legacy", id] as const,
  comments: (id: AssetId) => ["comments", id] as const,
  stats: ["stats"] as const,
  sources: ["sources"] as const,
  jobs: ["jobs"] as const,
  collections: ["collections"] as const,
  duplicates: ["duplicates"] as const,
  tags: ["tags"] as const,
  blocklist: ["blocklist"] as const,
  similar: (id: AssetId) => ["similar", id] as const,
  folders: (source: string, prefix: string) => ["folders", source, prefix] as const,
};

const PAGE_LIMIT = BROWSE_PAGE_SIZE;

export function useVersion() {
  return useQuery({ queryKey: qk.version, queryFn: api.version, staleTime: Infinity });
}

/** Who the current credential is + what it can do (front-door auth). Keyed on non-secret connection state so a
 *  sign-in / sign-out / token swap refetches automatically (the AuthGate reloads the app on those,
 *  but keying it makes the hook correct even without a reload). A 401 flips the AuthGate via the
 *  shared QueryCache handler; we don't retry it. Every server posture reports scopes, so the whole
 *  UI can gate consistently — an auth-off server grants full trust (all scopes). */
export function useWhoami() {
  return useQuery({
    queryKey: [...qk.whoami, getServer().base, hasBearerCredential() ? "bearer" : "session"],
    queryFn: api.whoami,
    staleTime: Infinity,
    retry: false,
  });
}

/** The full-trust scope set — what an auth-off server (or a not-yet-loaded whoami on such a server)
 *  grants. Every capability. */
const ALL_SCOPES: Scope[] = ["read", "write", "admin", "mcp_use", "federate"];

/** The caller's current scopes. On an auth-off server (`/whoami` absent/erroring with the local
 *  owner's full trust) this resolves to every scope, so gating never hides capability the operator
 *  actually has. While loading it is optimistic (all scopes) to avoid a flash of disabled controls;
 *  a real read-only token settles it once the query lands. */
export function useScopes(): Scope[] {
  const whoami = useWhoami();
  if (whoami.data) return whoami.data.scopes;
  // No data yet (loading) or the endpoint 404'd on an older/auth-off server: assume full trust.
  return ALL_SCOPES;
}

/** Does the caller hold `scope`? The shared gate for write-action controls (see useScopes). */
export function useCan(scope: Scope): boolean {
  return useScopes().includes(scope);
}

/** The browse grid/table — cursor-paginated infinite scroll (tech-spec 03 §4.1). With a `collection`
 *  the source switches to that collection's assets (manual: the member list; smart: the saved query
 *  resolved live) instead of the faceted search. Keyed under `qk.assets` either way, so the same WS
 *  asset events keep both views live. */
export function useAssets(req: QueryRequest, collection?: CollectionId | null) {
  // The query key owns the directory's lifetime. Page payloads are capped below, while this sparse
  // cursor chain is enough to recreate an evicted page when the user scrolls backwards.
  const cursorDirectoryKey = JSON.stringify([collection ?? null, req]);
  const cursorDirectory = browseCursorDirectory(cursorDirectoryKey);
  return useInfiniteQuery({
    queryKey: collection ? [...qk.assets, "collection", collection] : [...qk.assets, req],
    initialPageParam: { after: null, index: 0 } as BrowsePageParam,
    queryFn: async ({ pageParam }) => {
      const page = collection
        ? api.collectionAssets(collection, { after: pageParam.after, limit: PAGE_LIMIT })
        : api.query({ ...req, page: { after: pageParam.after, limit: PAGE_LIMIT } });
      const resolved = await page;
      cursorDirectory.remember(pageParam, resolved.cursor);
      return resolved;
    },
    getNextPageParam: (last, _pages, lastParam) =>
      last.cursor === null
        ? undefined
        : { after: last.cursor, index: lastParam.index + 1 },
    getPreviousPageParam: (_first, _pages, firstParam) =>
      cursorDirectory.previous(firstParam),
    maxPages: BROWSE_MAX_PAGES,
  });
}

export function useAsset(id: AssetId | null, source?: SourceId | null) {
  return useQuery({
    queryKey: id ? qk.asset(id, source) : ["asset", "none"],
    queryFn: () => api.getAsset(id as AssetId, source),
    enabled: !!id,
  });
}

/** Library aggregates, scoped to `source` when set (per-source sidebar counts, phase 6). A
 *  federated source's numbers come live from the peer, so they're polled — local WS events can't
 *  announce the peer's catalog changes. Keyed under `qk.stats` so WS invalidation still hits. */
export function useStats(source?: string | null, opts?: { isPeer?: boolean }) {
  return useQuery({
    queryKey: [...qk.stats, source ?? "all"],
    queryFn: () => api.stats(source),
    refetchInterval: opts?.isPeer ? 30_000 : false,
  });
}

/** "Find similar" for one asset — cosine over embeddings (phase 3). Opt-in (`enabled`) so the
 *  Inspector only queries when the user asks; cached per-asset so reopening is instant. */
export function useSimilar(id: AssetId | null, enabled: boolean) {
  return useQuery({
    queryKey: id ? qk.similar(id) : ["similar", "none"],
    queryFn: () => api.findSimilar({ asset: id as AssetId }),
    enabled: enabled && !!id,
    staleTime: 60_000,
  });
}

/** Folder navigation (issue #66): the immediate subfolders under `prefix` in one source — the lazy
 *  unit the sidebar tree expands. Enabled per-node only when that node is open, so an unexpanded
 *  source pulls nothing. Keyed by (source, prefix); ws.ts invalidates the `["folders"]` root when a
 *  scan or asset change reshapes the tree. */
export function useFolders(source: SourceId | null, prefix: string, enabled: boolean) {
  return useQuery({
    queryKey: source ? qk.folders(source, prefix) : ["folders", "none"],
    queryFn: () => api.listFolders({ source: source as SourceId, prefix }),
    enabled: enabled && !!source,
    staleTime: 30_000,
  });
}

export function useSources() {
  return useQuery({ queryKey: qk.sources, queryFn: api.listSources });
}

export function useJobs(req: JobListRequest = {}) {
  return useQuery({
    queryKey: [...qk.jobs, req],
    queryFn: () => api.listJobs(req),
    refetchInterval: 4000, // safety net alongside the WS push
  });
}

/** Durable newest-first history. Each page cursor is supplied by the server after applying the
 * caller's visibility ceiling, so restricted sessions never fetch or render hidden job details. */
export function useJobHistory() {
  return useInfiniteQuery({
    queryKey: [...qk.jobs, "history"],
    queryFn: ({ pageParam }) =>
      api.listJobs({ page: { limit: 50, after: pageParam } }),
    initialPageParam: null as string | null,
    getNextPageParam: (last) => last.cursor ?? undefined,
  });
}

/** Fetch the selected job's durable detail separately; list/event summaries intentionally omit
 * potentially large convert/export item reports. */
export function useJob(id: JobId | null) {
  return useQuery({
    queryKey: [...qk.jobs, "detail", id],
    queryFn: () => api.getJob(id as JobId),
    enabled: id != null,
  });
}

export function useAddSource() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: (req: AddSource) => api.addSource(req),
    meta: { success: "Source added", errorPrefix: "Couldn’t add source" },
    onSuccess: () => qc.invalidateQueries({ queryKey: qk.sources }),
  });
}

export function useRemoveSource() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: (id: SourceId) => api.removeSource(id, { keep_metadata: false }),
    meta: { success: "Source removed", errorPrefix: "Couldn’t remove source" },
    onSuccess: () => {
      qc.invalidateQueries({ queryKey: qk.sources });
      qc.invalidateQueries({ queryKey: qk.assets });
      qc.invalidateQueries({ queryKey: qk.stats });
      qc.invalidateQueries({ queryKey: qk.tags });
    },
  });
}

export function useScan() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: (req: ScanRequest) => api.submitScan(req),
    meta: { success: "Rescan started", errorPrefix: "Couldn’t start scan" },
    onSuccess: () => qc.invalidateQueries({ queryKey: qk.jobs }),
  });
}

/** Kick the analysis pass (embeddings, tileability/pHash, auto-tag suggestions) — library-wide or
 *  scoped to specific assets. Progress surfaces through the shared job plumbing (StatusBar). */
export function useAnalyze() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: (req: AnalyzeRequest) => api.submitAnalyze(req),
    meta: { success: "Analysis started", errorPrefix: "Couldn’t start analysis" },
    onSuccess: () => qc.invalidateQueries({ queryKey: qk.jobs }),
  });
}

/** Force a rebuild of the derived thumbnail(s) for specific assets: the server drops each asset's
 *  cached PNG + 3D preview so the next view re-renders from source (e.g. after a source file was
 *  edited in place). On success we bump each asset's regeneration epoch so its `<img>` re-fetches past
 *  the browser + response cache — the cache key (content hash) is otherwise unchanged. */
export function useRegenerateThumbnail() {
  return useMutation({
    mutationFn: (ids: AssetId[]) => api.regenerateThumbnails({ assets: ids }),
    meta: { success: "Thumbnail regenerated", errorPrefix: "Couldn’t regenerate thumbnail" },
    onSuccess: (_data, ids) => bustThumbnails(ids),
  });
}

/** Accept/reject an auto-tag suggestion; refresh the inspected asset + grid on success. */
export function useReviewSuggestion() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: (req: SuggestionReview) => api.reviewSuggestion(req),
    onSuccess: (_data, req) => {
      qc.invalidateQueries({ queryKey: qk.asset(req.asset) });
      qc.invalidateQueries({ queryKey: qk.assets });
      qc.invalidateQueries({ queryKey: qk.stats });
      qc.invalidateQueries({ queryKey: qk.tags });
    },
  });
}

/** Flag/unflag an asset as a favourite (issue #63); refresh the inspected asset + grid (so the
 *  Favorites filter and any star affordance re-render) on success. */
export function useSetFavorite() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: (req: FavoriteRequest) => api.setFavorite(req),
    meta: { errorPrefix: "Couldn’t update favourite" },
    onSuccess: (_data, req) => {
      qc.invalidateQueries({ queryKey: qk.asset(req.asset) });
      qc.invalidateQueries({ queryKey: qk.assets });
    },
  });
}

/** Set or clear an asset's free-text note (issue #81).
 *
 *  Deliberately **no `qk.asset` invalidation**: this mutation autosaves while the user is still
 *  typing, and a refetch would race the textarea and could stomp in-flight keystrokes with the
 *  server's older copy. The note is the only thing that changed and the response already carries
 *  what was stored, so `setQueryData` patches the cached asset in place instead. The grid is
 *  invalidated because a note feeds full-text search, so result sets can legitimately move. */
export function useSetNote() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: ({ id, body }: { id: AssetId; body: string }) => api.setNote(id, body),
    meta: { errorPrefix: "Couldn’t save note" },
    onSuccess: (note, { id }) => {
      qc.setQueryData(qk.asset(id), (prev: Asset | undefined) =>
        prev ? { ...prev, note } : prev,
      );
      qc.invalidateQueries({ queryKey: qk.assets });
    },
  });
}

// ── per-asset discussion (issue #82) ────────────────────────────────────────

/** An asset's thread. Only enabled once the caller knows accounts are on — the whole surface 404s
 *  otherwise, and a query that always fails would spam the error toast. */
export function useComments(id: AssetId | null, enabled: boolean) {
  return useQuery({
    queryKey: qk.comments(id ?? ""),
    queryFn: () => api.listComments(id as AssetId),
    enabled: !!id && enabled,
  });
}

/** Post / edit / delete, each refreshing just this asset's thread. Unlike the note editor there is
 *  no autosave race to protect against: a message is an explicit send, so refetching is exactly
 *  what the user expects to see. */
export function usePostComment(id: AssetId) {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: ({ body, replyTo }: { body: string; replyTo?: string }) =>
      api.postComment(id, body, replyTo),
    meta: { errorPrefix: "Couldn’t post" },
    onSuccess: () => qc.invalidateQueries({ queryKey: qk.comments(id) }),
  });
}

export function useEditComment(id: AssetId) {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: ({ commentId, body }: { commentId: string; body: string }) =>
      api.editComment(commentId, body),
    meta: { errorPrefix: "Couldn’t save the edit" },
    onSuccess: () => qc.invalidateQueries({ queryKey: qk.comments(id) }),
  });
}

export function useDeleteComment(id: AssetId) {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: (commentId: string) => api.deleteComment(commentId),
    meta: { errorPrefix: "Couldn’t delete" },
    onSuccess: () => qc.invalidateQueries({ queryKey: qk.comments(id) }),
  });
}

export function useEditTags() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: (req: TagEditRequest) => api.editTags(req),
    meta: { errorPrefix: "Couldn’t update tags" },
    onSuccess: (_result, req) => {
      if (req.dry_run) return;
      qc.invalidateQueries({ queryKey: qk.assets });
      qc.invalidateQueries({ queryKey: ["asset"] });
      qc.invalidateQueries({ queryKey: qk.stats });
    },
  });
}

export function useTagVocabulary(prefix = "") {
  return useQuery({
    queryKey: [...qk.tags, "vocabulary", prefix],
    queryFn: () => api.listTags(prefix),
    staleTime: 30_000,
  });
}

/** Duplicate groups for the review surface (exact or near). Refetches when the tier/media changes;
 *  ws.ts invalidates `qk.duplicates` on asset changes and finished jobs so new pHashes surface. */
export function useDuplicates(req: DupRequest) {
  return useInfiniteQuery({
    queryKey: [...qk.duplicates, req],
    queryFn: ({ pageParam }) => api.listDuplicates({ ...req, after: pageParam }),
    initialPageParam: null as string | null,
    getNextPageParam: (last) => last.cursor ?? undefined,
  });
}

/** Persist a keep/resolve/dismiss decision and any explicit catalog-only removals atomically. */
export function useReviewDuplicate() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: (req: DupReviewRequest) => api.reviewDuplicate(req),
    meta: { errorPrefix: "Couldn’t update duplicate review" },
    onSuccess: (_data, req) => {
      qc.invalidateQueries({ queryKey: qk.duplicates });
      if (req.removals?.length) {
        qc.invalidateQueries({ queryKey: qk.assets });
        qc.invalidateQueries({ queryKey: qk.stats });
      }
      if (req.removals?.some((removal) => removal.block)) {
        qc.invalidateQueries({ queryKey: qk.blocklist });
      }
    },
  });
}

/** Exact duplicate membership for only the local rows retained by Browser's bounded page window. */
export function useDuplicateMembership(assets: AssetId[]) {
  return useQuery({
    queryKey: [...qk.duplicates, "membership", assets],
    queryFn: () => api.duplicateMembership({ assets }),
    enabled: assets.length > 0,
  });
}

/** At most one capped exact group for the local asset currently open in Inspector. */
export function useDuplicateGroup(id: AssetId | null) {
  return useQuery({
    queryKey: [...qk.duplicates, "asset", id],
    queryFn: () => api.duplicateGroup(id as AssetId),
    enabled: id !== null,
  });
}

// ── remove / blocklist (issue #21) ──────────────────────────────────────────

/** Remove an asset; `block` also blocks its content hash from any future scan. Refreshes the grid,
 *  counts, dedup groups, and (when blocking) the blocklist view. */
export function useRemoveAsset() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: ({ id, block }: { id: AssetId; block?: boolean } & RemoveAsset) =>
      api.removeAsset(id, { block }),
    meta: { errorPrefix: "Couldn’t remove asset" },
    onSuccess: (_data, { block }) => {
      qc.invalidateQueries({ queryKey: qk.assets });
      qc.invalidateQueries({ queryKey: qk.stats });
      qc.invalidateQueries({ queryKey: qk.duplicates });
      if (block) qc.invalidateQueries({ queryKey: qk.blocklist });
    },
  });
}

/** The rescan blocklist — content hashes removed with "Remove + block". */
export function useBlocklist() {
  return useQuery({ queryKey: qk.blocklist, queryFn: api.listBlocklist });
}

/** Lift a block so the content can be re-imported by a later scan. */
export function useUnblock() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: (hash: ContentHash) => api.unblock(hash),
    meta: { success: "Block lifted", errorPrefix: "Couldn’t unblock" },
    onSuccess: () => qc.invalidateQueries({ queryKey: qk.blocklist }),
  });
}

// ── collections / smart folders ─────────────────────────────────────────────

export function useCollections() {
  return useQuery({ queryKey: qk.collections, queryFn: api.listCollections });
}

export function useCreateCollection() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: (req: NewCollection) => api.createCollection(req),
    meta: { success: "Collection created", errorPrefix: "Couldn’t create collection" },
    onSuccess: () => qc.invalidateQueries({ queryKey: qk.collections }),
  });
}

/** Rename a collection and/or replace a smart folder's saved query. Refresh the active browse too:
 * replacing the query changes its live result set immediately. */
export function useUpdateCollection() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: ({ id, patch }: { id: CollectionId; patch: UpdateCollection }) =>
      api.updateCollection(id, patch),
    meta: { success: "Collection updated", errorPrefix: "Couldn’t update collection" },
    onSuccess: () => {
      qc.invalidateQueries({ queryKey: qk.collections });
      qc.invalidateQueries({ queryKey: qk.assets });
    },
  });
}

export function useDeleteCollection() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: (id: CollectionId) => api.deleteCollection(id),
    meta: { success: "Collection deleted", errorPrefix: "Couldn’t delete collection" },
    onSuccess: () => qc.invalidateQueries({ queryKey: qk.collections }),
  });
}

/** Add/remove members of a manual collection. Refreshes the collection list (counts), the browse
 *  grid (collection views live under `qk.assets`), and the inspected asset (its `collections`). */
export function useCollectionMembers() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: ({ id, members }: { id: CollectionId; members: CollectionMembers }) =>
      api.modifyCollectionMembers(id, members),
    meta: { errorPrefix: "Couldn’t update collection" },
    onSuccess: () => {
      qc.invalidateQueries({ queryKey: qk.collections });
      qc.invalidateQueries({ queryKey: qk.assets });
      qc.invalidateQueries({ queryKey: ["asset"] });
    },
  });
}

/** Export a manifest (json/csv/sidecar). Read-only w.r.t. the catalog, so no cache invalidation;
 *  the caller shows the returned report (path + counts). */
export function useExport() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: async (req: ExportRequest) => (await api.submitExport(req)).job_id,
    onSuccess: () => qc.invalidateQueries({ queryKey: qk.jobs }),
    meta: { errorPrefix: "Export failed" },
  });
}

/** Convert assets to a target format. Writes to an output dir outside any source, so the catalog is
 *  unchanged — no invalidation; the caller shows the per-item report. */
export function useConvert() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: async (req: ConvertRequest) => (await api.submitConvert(req)).job_id,
    onSuccess: () => qc.invalidateQueries({ queryKey: qk.jobs }),
    meta: { errorPrefix: "Convert failed" },
  });
}

export function useCancelJob() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: (id: string) => api.cancelJob(id),
    onSuccess: () => qc.invalidateQueries({ queryKey: qk.jobs }),
  });
}
