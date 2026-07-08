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
  /** Favorites-only (issue #63) — a boolean facet that composes with media/source/license/tag. */
  fav: boolean;
  /** Advanced Search: structured attribute filters (typed dropdown/range/toggle controls over the
   *  per-media attr columns) plus tag filters. Carried as a JSON `Filter[]` in the `adv` URL param so
   *  a whole faceted query stays linkable. AND-ed with the sidebar facets and text search. */
  adv: Filter[];
  sort: SortField;
  dir: SortDir;
  /** Text-search strategy (semantic-search M5). `lexical` is the default FTS+synonym path;
   *  `hybrid`/`semantic` widen with embedding neighbours of the matches. */
  mode: SearchMode;
  view: ViewMode;
  selected: string | null;
}

/** Decode the `adv` URL param (a JSON `Filter[]`). Malformed input degrades to no advanced filters
 *  rather than throwing — a hand-edited URL never breaks the browse. */
function parseAdv(raw: string | null): Filter[] {
  if (!raw) return [];
  try {
    const v = JSON.parse(raw);
    return Array.isArray(v) ? (v as Filter[]) : [];
  } catch {
    return [];
  }
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
      fav: params.get("fav") === "1",
      adv: parseAdv(params.get("adv")),
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
          if ("fav" in next) set("fav", next.fav ? "1" : null);
          if ("adv" in next) set("adv", next.adv && next.adv.length ? JSON.stringify(next.adv) : null);
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
    if (state.fav) filters.push({ field: "favorite", op: "eq", value: { bool: true } });
    // Advanced Search structured/tag filters, AND-ed onto the sidebar facets.
    filters.push(...state.adv);
    return {
      text: state.q || null,
      filters,
      sort: { field: state.sort, dir: state.dir },
      mode: state.mode,
    };
  }, [state]);

  return { state, patch, request };
}
