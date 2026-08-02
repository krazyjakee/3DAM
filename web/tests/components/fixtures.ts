import type { Asset, AssetSummary, Page } from "../../src/api/types";

export function assetSummary(overrides: Partial<AssetSummary> = {}): AssetSummary {
  return {
    id: "asset-a",
    name: "Albedo.png",
    media: "image",
    format: "png",
    size: 1024,
    license: { id: null, status: "unknown" },
    top_tags: [],
    origin: "local",
    key_attrs: { dimensions: "64 × 64" },
    favorite: false,
    source_id: "source-a",
    ...overrides,
  };
}

export function asset(overrides: Partial<Asset> = {}): Asset {
  const summary = overrides.summary ?? assetSummary();
  return {
    summary,
    hash: "a".repeat(64),
    source_id: summary.source_id ?? "source-a",
    path: summary.name,
    timestamps: { created: 1, modified: 2, scanned: 3, analyzed: 4 },
    attributes: {
      media: "image",
      width: 64,
      height: 64,
      has_alpha: true,
      color_space: "srgb",
    },
    license: {
      id: null,
      status: "unknown",
      commercial: null,
      modify: null,
      redistribute: null,
      attribution: null,
      holder: null,
      credit: null,
      url: null,
      provenance: "test",
    },
    tags: [],
    collections: [],
    note: null,
    ...overrides,
  };
}

export function assetPage(items: AssetSummary[]): Page<AssetSummary> {
  return {
    items,
    cursor: null,
    total: items.length,
    partial: { complete: true },
  };
}
