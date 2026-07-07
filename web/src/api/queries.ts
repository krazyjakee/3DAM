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
import type {
  AddSource,
  AnalyzeRequest,
  AssetId,
  CollectionId,
  CollectionMembers,
  ContentHash,
  ConvertRequest,
  DupRequest,
  ExportRequest,
  JobListRequest,
  NewCollection,
  QueryRequest,
  RemoveAsset,
  ScanRequest,
  SourceId,
  SuggestionReview,
} from "./types";

/** Stable query-key roots — ws.ts invalidates against these. */
export const qk = {
  version: ["version"] as const,
  assets: ["assets"] as const,
  asset: (id: AssetId) => ["asset", id] as const,
  stats: ["stats"] as const,
  sources: ["sources"] as const,
  jobs: ["jobs"] as const,
  collections: ["collections"] as const,
  duplicates: ["duplicates"] as const,
  blocklist: ["blocklist"] as const,
  similar: (id: AssetId) => ["similar", id] as const,
};

const PAGE_LIMIT = 60;

export function useVersion() {
  return useQuery({ queryKey: qk.version, queryFn: api.version, staleTime: Infinity });
}

/** The browse grid/table — cursor-paginated infinite scroll (tech-spec 03 §4.1). With a `collection`
 *  the source switches to that collection's assets (manual: the member list; smart: the saved query
 *  resolved live) instead of the faceted search. Keyed under `qk.assets` either way, so the same WS
 *  asset events keep both views live. */
export function useAssets(req: QueryRequest, collection?: CollectionId | null) {
  return useInfiniteQuery({
    queryKey: collection ? [...qk.assets, "collection", collection] : [...qk.assets, req],
    initialPageParam: null as string | null,
    queryFn: ({ pageParam }) =>
      collection
        ? api.collectionAssets(collection, { after: pageParam, limit: PAGE_LIMIT })
        : api.query({ ...req, page: { after: pageParam, limit: PAGE_LIMIT } }),
    getNextPageParam: (last) => last.cursor ?? undefined,
  });
}

export function useAsset(id: AssetId | null) {
  return useQuery({
    queryKey: id ? qk.asset(id) : ["asset", "none"],
    queryFn: () => api.getAsset(id as AssetId),
    enabled: !!id,
  });
}

export function useStats() {
  return useQuery({ queryKey: qk.stats, queryFn: api.stats });
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

/** Accept/reject an auto-tag suggestion; refresh the inspected asset + grid on success. */
export function useReviewSuggestion() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: (req: SuggestionReview) => api.reviewSuggestion(req),
    onSuccess: (_data, req) => {
      qc.invalidateQueries({ queryKey: qk.asset(req.asset) });
      qc.invalidateQueries({ queryKey: qk.assets });
    },
  });
}

/** Duplicate groups for the review surface (exact or near). Refetches when the tier/media changes;
 *  ws.ts invalidates `qk.duplicates` on asset changes and finished jobs so new pHashes surface. */
export function useDuplicates(req: DupRequest) {
  return useQuery({
    queryKey: [...qk.duplicates, req],
    queryFn: () => api.listDuplicates(req),
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

/** Rename a collection (the only field the web UI edits directly; a smart folder's query is set
 *  at creation via the CLI in v1). */
export function useRenameCollection() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: ({ id, name }: { id: CollectionId; name: string }) =>
      api.updateCollection(id, { name }),
    meta: { errorPrefix: "Couldn’t rename collection" },
    onSuccess: () => qc.invalidateQueries({ queryKey: qk.collections }),
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
  return useMutation({
    mutationFn: (req: ExportRequest) => api.exportAssets(req),
    meta: { errorPrefix: "Export failed" },
  });
}

/** Convert assets to a target format. Writes to an output dir outside any source, so the catalog is
 *  unchanged — no invalidation; the caller shows the per-item report. */
export function useConvert() {
  return useMutation({
    mutationFn: (req: ConvertRequest) => api.convert(req),
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
