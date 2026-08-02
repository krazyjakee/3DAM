import {
  createContext,
  useCallback,
  useContext,
  useEffect,
  useMemo,
  useRef,
  useState,
  type ReactNode,
} from "react";
import type { AssetSummary, QueryRequest } from "@/api/types";
import { useViewState } from "./view-state";

/** Asset ids are peer-local, so every selection identity includes its federated owner. */
export interface AssetRef {
  id: string;
  owner: string | null;
}

export type ResultSelector =
  | { kind: "query"; query: QueryRequest }
  | { kind: "collection"; collection: string };

interface SelectedEntry {
  ref: AssetRef;
  asset?: AssetSummary;
}

interface ExplicitSelection {
  kind: "explicit";
  entries: Map<string, SelectedEntry>;
}

interface ResultSelection {
  kind: "results";
  selector: ResultSelector;
  total: number;
  /** Loaded representatives are only for styling/focus; the selector is authoritative. */
  loaded: Map<string, AssetSummary>;
}

export type SelectionScope = ExplicitSelection | ResultSelection;

interface SelectionState {
  focused: AssetRef | null;
  selected: SelectionScope;
  anchor: string | null;
  browseScope: string | null;
}

export interface SelectionModel {
  focused: AssetRef | null;
  selected: SelectionScope;
  count: number;
  explicitAssets: AssetSummary[];
  isSelected: (asset: AssetSummary) => boolean;
  selectAsset: (asset: AssetSummary, mods: { meta: boolean; shift: boolean }, order: AssetSummary[]) => void;
  focusAsset: (asset: AssetSummary) => void;
  selectExplicit: (assets: AssetSummary[]) => void;
  selectResults: (selector: ResultSelector, total: number, loaded: AssetSummary[]) => void;
  clear: () => void;
  reconcile: (options: {
    browseScope: string;
    loaded: AssetSummary[];
    visible: AssetSummary[];
    selector: ResultSelector;
    total: number | null;
  }) => void;
}

const SelectionContext = createContext<SelectionModel | null>(null);

export function assetRef(asset: AssetSummary): AssetRef {
  return {
    id: asset.id,
    owner: typeof asset.origin === "object" ? asset.source_id : null,
  };
}

export function assetKey(ref: AssetRef): string {
  return JSON.stringify([ref.owner, ref.id]);
}

export function assetSelectionKey(asset: AssetSummary): string {
  return assetKey(assetRef(asset));
}

function sameRef(left: AssetRef | null, right: AssetRef | null): boolean {
  return left?.id === right?.id && left?.owner === right?.owner;
}

function emptySelection(): ExplicitSelection {
  return { kind: "explicit", entries: new Map() };
}

function initialState(focused: AssetRef | null): SelectionState {
  const selected = emptySelection();
  if (focused) selected.entries.set(assetKey(focused), { ref: focused });
  return {
    focused,
    selected,
    anchor: focused ? assetKey(focused) : null,
    browseScope: null,
  };
}

/** One workspace selection model. Focus mirrors `?sel=&owner=` for deep links; the selected scope
 * stays in memory because it may contain many composite refs or a server-side query selector. */
export function SelectionProvider({ children }: { children: ReactNode }) {
  const { state: view, patch } = useViewState();
  const urlFocus = useMemo<AssetRef | null>(
    () => (view.selected ? { id: view.selected, owner: view.owner } : null),
    [view.selected, view.owner],
  );
  const [state, setState] = useState<SelectionState>(() => initialState(urlFocus));
  const stateRef = useRef(state);
  stateRef.current = state;

  const publish = useCallback((next: SelectionState) => {
    stateRef.current = next;
    setState(next);
  }, []);

  const commit = useCallback(
    (next: SelectionState, previous: SelectionState = stateRef.current) => {
      publish(next);
      if (!sameRef(previous.focused, next.focused)) {
        patch({ selected: next.focused?.id ?? null, owner: next.focused?.owner ?? null });
      }
    },
    [patch, publish],
  );

  // Inspector links and browser history can change focus outside the selection controls. Adopt that
  // focus as a one-item explicit selection unless it is already inside the current scope.
  useEffect(() => {
    const current = stateRef.current;
    if (sameRef(current.focused, urlFocus)) return;
    if (!urlFocus) {
      publish({ ...current, focused: null, selected: emptySelection(), anchor: null });
      return;
    }
    const key = assetKey(urlFocus);
    if (current.selected.kind === "explicit" && current.selected.entries.has(key)) {
      publish({ ...current, focused: urlFocus });
      return;
    }
    publish(
      {
        ...current,
        focused: urlFocus,
        selected: { kind: "explicit", entries: new Map([[key, { ref: urlFocus }]]) },
        anchor: key,
      },
    );
  }, [urlFocus, publish]);

  const clear = useCallback(() => {
    const current = stateRef.current;
    commit({ ...current, focused: null, selected: emptySelection(), anchor: null }, current);
  }, [commit]);

  const selectExplicit = useCallback(
    (assets: AssetSummary[]) => {
      const current = stateRef.current;
      const entries = new Map(
        assets.map((asset) => {
          const ref = assetRef(asset);
          return [assetKey(ref), { ref, asset }] as const;
        }),
      );
      const retainedFocus = current.focused && entries.has(assetKey(current.focused));
      const focused = retainedFocus ? current.focused : (entries.values().next().value?.ref ?? null);
      commit(
        {
          ...current,
          focused,
          selected: { kind: "explicit", entries },
          anchor: focused ? assetKey(focused) : null,
        },
        current,
      );
    },
    [commit],
  );

  const selectAsset = useCallback(
    (asset: AssetSummary, mods: { meta: boolean; shift: boolean }, order: AssetSummary[]) => {
      const current = stateRef.current;
      const ref = assetRef(asset);
      const key = assetKey(ref);
      const entries =
        current.selected.kind === "explicit"
          ? new Map(current.selected.entries)
          : new Map<string, SelectedEntry>();

      if (mods.shift && current.anchor) {
        const keys = order.map(assetSelectionKey);
        const from = keys.indexOf(current.anchor);
        const to = keys.indexOf(key);
        if (from >= 0 && to >= 0) {
          const [start, end] = from < to ? [from, to] : [to, from];
          for (const item of order.slice(start, end + 1)) {
            const itemRef = assetRef(item);
            entries.set(assetKey(itemRef), { ref: itemRef, asset: item });
          }
        } else {
          entries.set(key, { ref, asset });
        }
      } else if (mods.meta) {
        if (entries.has(key)) entries.delete(key);
        else entries.set(key, { ref, asset });
      } else {
        entries.clear();
        entries.set(key, { ref, asset });
      }

      const focused = entries.has(key)
        ? ref
        : current.focused && entries.has(assetKey(current.focused))
          ? current.focused
          : (Array.from(entries.values()).at(-1)?.ref ?? null);
      commit(
        {
          ...current,
          focused,
          selected: { kind: "explicit", entries },
          anchor: mods.shift ? current.anchor : key,
        },
        current,
      );
    },
    [commit],
  );

  const focusAsset = useCallback(
    (asset: AssetSummary) => {
      const current = stateRef.current;
      const ref = assetRef(asset);
      const key = assetKey(ref);
      if (
        current.selected.kind === "explicit" &&
        current.selected.entries.has(key)
      ) {
        commit({ ...current, focused: ref }, current);
      } else if (current.selected.kind === "results") {
        commit({ ...current, focused: ref }, current);
      } else {
        selectExplicit([asset]);
      }
    },
    [commit, selectExplicit],
  );

  const selectResults = useCallback(
    (selector: ResultSelector, total: number, loaded: AssetSummary[]) => {
      const current = stateRef.current;
      const loadedMap = new Map(loaded.map((asset) => [assetSelectionKey(asset), asset]));
      const focused =
        current.focused && loadedMap.has(assetKey(current.focused))
          ? current.focused
          : loaded[0]
            ? assetRef(loaded[0])
            : null;
      commit(
        {
          ...current,
          focused,
          selected: { kind: "results", selector, total, loaded: loadedMap },
          anchor: null,
        },
        current,
      );
    },
    [commit],
  );

  const reconcile = useCallback(
    ({ browseScope, loaded, visible, selector, total }: Parameters<SelectionModel["reconcile"]>[0]) => {
      const current = stateRef.current;
      if (current.browseScope !== null && current.browseScope !== browseScope) {
        commit(
          { focused: null, selected: emptySelection(), anchor: null, browseScope },
          current,
        );
        return;
      }

      const loadedMap = new Map(loaded.map((asset) => [assetSelectionKey(asset), asset]));
      const visibleKeys = new Set(visible.map(assetSelectionKey));
      let focused = current.focused;
      let selected = current.selected;
      let anchor = current.anchor;
      let changed = current.browseScope !== browseScope;

      if (selected.kind === "explicit") {
        const entries = new Map(selected.entries);
        for (const [key, entry] of entries) {
          const loadedAsset = loadedMap.get(key);
          // A loaded row hidden by duplicate collapsing is no longer an actionable representative.
          if (loadedAsset && !visibleKeys.has(key)) {
            entries.delete(key);
            changed = true;
          } else if (loadedAsset && entry.asset !== loadedAsset) {
            entries.set(key, { ...entry, asset: loadedAsset });
            changed = true;
          }
        }
        if (focused && !entries.has(assetKey(focused)) && entries.size > 0) {
          focused = Array.from(entries.values()).at(-1)?.ref ?? null;
          changed = true;
        } else if (entries.size === 0 && focused) {
          focused = null;
          anchor = null;
          changed = true;
        }
        if (changed) selected = { kind: "explicit", entries };
      } else {
        const nextTotal = total ?? selected.total;
        const selectorChanged = JSON.stringify(selected.selector) !== JSON.stringify(selector);
        if (selectorChanged) {
          // Never turn "all results from A" into "all results from B" merely because a debounced
          // query caught up. A result-wide selection is renewed only by an explicit user action.
          commit(
            { focused: null, selected: emptySelection(), anchor: null, browseScope },
            current,
          );
          return;
        }
        if (nextTotal !== selected.total) {
          selected = { ...selected, total: nextTotal };
          changed = true;
        }
      }

      if (!changed) return;
      commit({ focused, selected, anchor, browseScope }, current);
    },
    [commit],
  );

  const value = useMemo<SelectionModel>(() => {
    const count =
      state.selected.kind === "explicit" ? state.selected.entries.size : state.selected.total;
    const explicitAssets =
      state.selected.kind === "explicit"
        ? Array.from(state.selected.entries.values()).flatMap((entry) =>
            entry.asset ? [entry.asset] : [],
          )
        : [];
    return {
      focused: state.focused,
      selected: state.selected,
      count,
      explicitAssets,
      isSelected: (asset) =>
        state.selected.kind === "results" ||
        state.selected.entries.has(assetSelectionKey(asset)),
      selectAsset,
      focusAsset,
      selectExplicit,
      selectResults,
      clear,
      reconcile,
    };
  }, [state, selectAsset, focusAsset, selectExplicit, selectResults, clear, reconcile]);

  return <SelectionContext.Provider value={value}>{children}</SelectionContext.Provider>;
}

export function useSelection(): SelectionModel {
  const model = useContext(SelectionContext);
  if (!model) throw new Error("useSelection must be used within <SelectionProvider>");
  return model;
}
