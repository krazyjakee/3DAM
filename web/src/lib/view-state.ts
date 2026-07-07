// URL owns view state (tech-spec 09 §B.2): current media/source/tag filter, text search, sort,
// grid⇄table view, and the selected asset. Views are linkable and back/forward works. Components
// read this and derive the file-03 `QueryRequest`; nothing view-related lives in a global store.

import { useCallback, useMemo } from "react";
import { useSearchParams } from "react-router-dom";
import type {
  Filter,
  MediaType,
  QueryRequest,
  SearchMode,
  SortDir,
  SortField,
} from "@/api/types";

export type ViewMode = "grid" | "table";

export interface ViewState {
  q: string;
  media: MediaType | null;
  source: string | null;
  license: string | null;
  /** Confirmed-tag facet — composes with media/source/license, mutually exclusive with a collection. */
  tag: string | null;
  /** When set, the Browser shows this collection's assets instead of the faceted search. Mutually
   *  exclusive with the facet filters — selecting one clears the other (see Navigation). */
  collection: string | null;
  sort: SortField;
  dir: SortDir;
  /** Text-search strategy (semantic-search M5). `lexical` is the default FTS+synonym path;
   *  `hybrid`/`semantic` widen with embedding neighbours of the matches. */
  mode: SearchMode;
  view: ViewMode;
  selected: string | null;
}

export function useViewState() {
  const [params, setParams] = useSearchParams();

  const state: ViewState = useMemo(
    () => ({
      q: params.get("q") ?? "",
      media: (params.get("media") as MediaType | null) || null,
      source: params.get("source"),
      license: params.get("license"),
      tag: params.get("tag"),
      collection: params.get("col"),
      sort: (params.get("sort") as SortField | null) ?? "name",
      dir: (params.get("dir") as SortDir | null) ?? "asc",
      mode: (params.get("mode") as SearchMode | null) ?? "lexical",
      view: (params.get("view") as ViewMode | null) ?? "grid",
      selected: params.get("sel"),
    }),
    [params],
  );

  const patch = useCallback(
    (next: Partial<ViewState>) => {
      setParams(
        (prev) => {
          const p = new URLSearchParams(prev);
          const set = (k: string, v: string | null | undefined) => {
            if (v === null || v === undefined || v === "") p.delete(k);
            else p.set(k, v);
          };
          if ("q" in next) set("q", next.q);
          if ("media" in next) set("media", next.media);
          if ("source" in next) set("source", next.source);
          if ("license" in next) set("license", next.license);
          if ("tag" in next) set("tag", next.tag);
          if ("collection" in next) set("col", next.collection);
          if ("sort" in next) set("sort", next.sort);
          if ("dir" in next) set("dir", next.dir);
          if ("mode" in next) set("mode", next.mode === "lexical" ? null : next.mode);
          if ("view" in next) set("view", next.view);
          if ("selected" in next) set("sel", next.selected);
          return p;
        },
        { replace: true },
      );
    },
    [setParams],
  );

  const request: QueryRequest = useMemo(() => {
    const filters: Filter[] = [];
    if (state.media) filters.push({ field: "media_type", op: "eq", value: { str: state.media } });
    if (state.source) filters.push({ field: "source", op: "eq", value: { str: state.source } });
    if (state.license)
      filters.push({ field: "license", op: "eq", value: { str: state.license } });
    if (state.tag) filters.push({ field: "tag", op: "eq", value: { str: state.tag } });
    return {
      text: state.q || null,
      filters,
      sort: { field: state.sort, dir: state.dir },
      mode: state.mode,
    };
  }, [state]);

  return { state, patch, request };
}
