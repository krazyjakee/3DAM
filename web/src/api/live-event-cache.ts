import type { QueryClient, QueryKey } from "@tanstack/react-query";
import type { AssetSummary, JobStatus, LibraryEvent, SourceInfo } from "./types";

const ROOTS = {
  assets: ["assets"],
  stats: ["stats"],
  sources: ["sources"],
  jobs: ["jobs"],
  duplicates: ["duplicates"],
  asset: ["asset"],
  comments: ["comments"],
  similar: ["similar"],
  folders: ["folders"],
  tags: ["tags"],
} as const satisfies Record<string, QueryKey>;

type Family = keyof typeof ROOTS;
type Timer = ReturnType<typeof setTimeout>;

const ASSET_FAMILIES: readonly Family[] = [
  "assets",
  "stats",
  "duplicates",
  "asset",
  "comments",
  "similar",
  "folders",
  "tags",
];
const JOB_BOUNDARY_FAMILIES: readonly Family[] = [...ASSET_FAMILIES, "sources", "jobs"];
const ALL_FAMILIES = Object.keys(ROOTS) as Family[];

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null;
}

function isLocalAsset(value: unknown, id: string): boolean {
  return isRecord(value) && value.id === id && value.origin === "local";
}

function replaceExistingAssetInData(data: unknown, asset: AssetSummary): unknown {
  if (!isRecord(data)) return data;
  if (Array.isArray(data.pages)) {
    let changed = false;
    const pages = data.pages.map((page) => {
      const next = replaceExistingAssetInData(page, asset);
      changed ||= next !== page;
      return next;
    });
    return changed ? { ...data, pages } : data;
  }
  if (!Array.isArray(data.items)) return data;
  let changed = false;
  const items = data.items.map((item) => {
    if (isLocalAsset(item, asset.id)) {
      changed = true;
      return asset;
    }
    return item;
  });
  return changed ? { ...data, items } : data;
}

/** Removal is query-independent: an absent asset cannot belong to any cached result set. */
function removeAssetFromData(data: unknown, id: string): unknown {
  if (!isRecord(data)) return data;
  if (Array.isArray(data.pages)) {
    const found = data.pages.some(
      (page) => isRecord(page) && Array.isArray(page.items) && page.items.some((item) => isLocalAsset(item, id)),
    );
    if (!found) return data;
    return {
      ...data,
      pages: data.pages.map((page) => {
        if (!isRecord(page) || !Array.isArray(page.items)) return page;
        return {
          ...page,
          items: page.items.filter((item) => !isLocalAsset(item, id)),
          total: typeof page.total === "number" ? Math.max(0, page.total - 1) : page.total,
        };
      }),
    };
  }
  if (!Array.isArray(data.items)) return data;
  const items = data.items.filter((item) => !isLocalAsset(item, id));
  if (items.length === data.items.length) return data;
  return {
    ...data,
    items,
    total: typeof data.total === "number" ? Math.max(0, data.total - 1) : data.total,
  };
}

function replaceJobInData(data: unknown, job: JobStatus): unknown {
  if (!isRecord(data)) return data;
  if (data.id === job.id && "progress" in data) return { ...data, ...job };
  if (Array.isArray(data.pages)) {
    let changed = false;
    const pages = data.pages.map((page) => {
      const next = replaceJobInData(page, job);
      changed ||= next !== page;
      return next;
    });
    return changed ? { ...data, pages } : data;
  }
  if (!Array.isArray(data.items)) return data;
  let changed = false;
  const items = data.items.map((item) => {
    if (isRecord(item) && item.id === job.id) {
      changed = true;
      return { ...item, ...job };
    }
    return item;
  });
  return changed ? { ...data, items } : data;
}

function hasJobInData(data: unknown, id: string): boolean {
  if (!isRecord(data)) return false;
  if (data.id === id && "progress" in data) return true;
  if (Array.isArray(data.pages)) return data.pages.some((page) => hasJobInData(page, id));
  return Array.isArray(data.items) && data.items.some((item) => isRecord(item) && item.id === id);
}

function patchSourceState(data: unknown, id: string, state: SourceInfo["state"]): unknown {
  if (!Array.isArray(data)) return data;
  let changed = false;
  const sources = data.map((source) => {
    if (isRecord(source) && source.id === id) {
      changed = true;
      return { ...source, state };
    }
    return source;
  });
  return changed ? sources : data;
}

/**
 * Coalesces a live-event storm into fixed query-family work. The pending set is bounded by the
 * number of known families and the timer is armed once (not reset per event), so 100k events retain
 * the same scheduler/cache bookkeeping as one event.
 */
export class LiveEventCacheBatcher {
  private readonly client: QueryClient;
  private readonly windowMs: number;
  private readonly dirty = new Set<Family>();
  private readonly activeJobs = new Set<string>();
  private timer: Timer | undefined;
  private jobTimer: Timer | undefined;
  private jobsDirty = false;
  private disposed = false;

  constructor(client: QueryClient, windowMs = 120) {
    this.client = client;
    this.windowMs = windowMs;
  }

  handle(event: LibraryEvent): Promise<void> | undefined {
    if (this.disposed) return;
    switch (event.type) {
      case "asset_added": {
        const asset = { ...event } as AssetSummary & { type?: string };
        delete asset.type;
        this.client.setQueriesData({ queryKey: ROOTS.assets }, (data) =>
          replaceExistingAssetInData(data, asset),
        );
        this.mark(ASSET_FAMILIES);
        return;
      }
      case "asset_changed":
        this.mark(ASSET_FAMILIES);
        return;
      case "asset_removed":
        this.client.setQueriesData({ queryKey: ROOTS.assets }, (data) =>
          removeAssetFromData(data, event.id),
        );
        this.client.removeQueries({ queryKey: ["asset", "local-or-legacy", event.id] });
        if (event.source_id !== null) {
          this.client.removeQueries({ queryKey: ["asset", event.source_id, event.id] });
        }
        this.mark(ASSET_FAMILIES);
        return;
      case "source_state":
        this.client.setQueryData(ROOTS.sources, (data) =>
          patchSourceState(data, event.id, event.state),
        );
        this.mark(["sources"]);
        return;
      case "job_progress": {
        const job = { ...event } as JobStatus & { type?: string };
        delete job.type;
        const cached = this.client
          .getQueriesData({ queryKey: ROOTS.jobs })
          .some(([, data]) => hasJobInData(data, event.id));
        this.client.setQueriesData({ queryKey: ROOTS.jobs }, (data) =>
          replaceJobInData(data, job),
        );
        if (!cached) this.scheduleJobsRefresh();
        if (event.state === "queued" || event.state === "running" || event.state === "paused") {
          this.activeJobs.add(event.id);
          if (this.timer !== undefined) clearTimeout(this.timer);
          this.timer = undefined;
          return;
        }
        this.activeJobs.delete(event.id);
        this.mark(JOB_BOUNDARY_FAMILIES, false);
        if (this.activeJobs.size === 0) return this.flush();
        return;
      }
      case "stream_lagged":
      case "catalog_reset":
        return this.resync();
    }
  }

  /** A reopened socket has an unknowable gap even when neither endpoint reported queue lag. */
  resync(): Promise<void> {
    if (this.disposed) return Promise.resolve();
    this.clearTimer();
    this.clearJobTimer();
    this.dirty.clear();
    this.jobsDirty = false;
    this.activeJobs.clear();
    return this.invalidate(ALL_FAMILIES);
  }

  flush(): Promise<void> {
    if (this.disposed || this.dirty.size === 0) return Promise.resolve();
    this.clearTimer();
    const families = [...this.dirty];
    this.dirty.clear();
    if (families.includes("jobs")) {
      this.clearJobTimer();
      this.jobsDirty = false;
    }
    return this.invalidate(families);
  }

  /** Exposed for deterministic tests; production normally reaches this through the short timer. */
  flushJobs(): Promise<void> {
    if (this.disposed || !this.jobsDirty) return Promise.resolve();
    this.clearJobTimer();
    this.jobsDirty = false;
    return this.invalidate(["jobs"]);
  }

  dispose(): void {
    this.disposed = true;
    this.clearTimer();
    this.clearJobTimer();
    this.dirty.clear();
    this.jobsDirty = false;
    this.activeJobs.clear();
  }

  private mark(families: readonly Family[], schedule = true): void {
    for (const family of families) this.dirty.add(family);
    if (!schedule || this.activeJobs.size > 0 || this.timer !== undefined || this.disposed) return;
    this.timer = setTimeout(() => {
      this.timer = undefined;
      void this.flush();
    }, this.windowMs);
  }

  private clearTimer(): void {
    if (this.timer !== undefined) clearTimeout(this.timer);
    this.timer = undefined;
  }

  private scheduleJobsRefresh(): void {
    this.jobsDirty = true;
    if (this.jobTimer !== undefined || this.disposed) return;
    this.jobTimer = setTimeout(() => {
      this.jobTimer = undefined;
      void this.flushJobs();
    }, this.windowMs);
  }

  private clearJobTimer(): void {
    if (this.jobTimer !== undefined) clearTimeout(this.jobTimer);
    this.jobTimer = undefined;
  }

  private async invalidate(families: readonly Family[]): Promise<void> {
    await Promise.all(
      families.map((family) => this.client.invalidateQueries({ queryKey: ROOTS[family] })),
    );
  }
}
