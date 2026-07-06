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
  AssetId,
  JobListRequest,
  QueryRequest,
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
};

const PAGE_LIMIT = 60;

export function useVersion() {
  return useQuery({ queryKey: qk.version, queryFn: api.version, staleTime: Infinity });
}

/** The browse grid/table — cursor-paginated infinite scroll (tech-spec 03 §4.1). */
export function useAssets(req: QueryRequest) {
  return useInfiniteQuery({
    queryKey: [...qk.assets, req],
    initialPageParam: null as string | null,
    queryFn: ({ pageParam }) =>
      api.query({ ...req, page: { after: pageParam, limit: PAGE_LIMIT } }),
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
    onSuccess: () => qc.invalidateQueries({ queryKey: qk.sources }),
  });
}

export function useRemoveSource() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: (id: SourceId) => api.removeSource(id, { keep_metadata: false }),
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

export function useCancelJob() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: (id: string) => api.cancelJob(id),
    onSuccess: () => qc.invalidateQueries({ queryKey: qk.jobs }),
  });
}
