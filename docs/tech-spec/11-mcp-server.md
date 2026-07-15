# 11 — MCP server

Status: **Draft v0.1** · Scope: the `rmcp`-based MCP server — Streamable HTTP on the shared `serve` port + `3dam mcp` stdio, the in-process `LibraryService` adapter, the tools/resources/prompts inventory, and write-gating / flag-removal.

This file is the low-level companion to [PRODUCT_SPEC.md](../PRODUCT_SPEC.md) §6.10 (the MCP server) and implements the mechanics of [ADR 0003](../adr/0003-mcp-server.md) (one port, in-process over `LibraryService`). It does **not** re-decide anything ADR 0003 settled — that the MCP surface is a frontend over `LibraryService` and not a subprocess/stdout scrape (contrast MoGen), that the primary transport is Streamable HTTP mounted on the `serve` port, that stdio is a secondary local path, and that all three MCP primitives are exposed — it cites those and fills in the adapter, the tool/resource/prompt definitions, and the gating flow.

It sits on top of, and stays inside the borders of, its neighbours:

- **[03-library-service-and-api.md](03-library-service-and-api.md)** owns the `LibraryService` trait, its DTOs, the error model, and pagination/streaming. This file **adapts** that trait to MCP — every tool is a thin call into it. It does not add engine methods.
- **[09-server-and-web-client.md](09-server-and-web-client.md)** owns the axum server, the bind/port, and the router. This file defines the **`POST /mcp` handler and the tool surface** that 09 mounts as one more route.
- **[10-auth-accounts-and-flags.md](10-auth-accounts-and-flags.md)** owns auth, scopes, and the feature-flag store. This file **consumes** the auth context and the tri-state `McpServer` flag (`Off` | `ReadOnly` | `ReadWrite`, file 10); it does not define it. (The config-file `[mcp] enabled=…, writes=…` block is the file-level spelling that *seeds* this flag — file 10 owns reconciliation — so the two spellings coexist as config-surface vs stored-flag, not as a contradiction.)
- **[13-cli.md](13-cli.md)** owns the `3dam mcp` CLI verb. This file defines **what that verb runs** (the stdio transport over an embedded engine).

---

## 1. Shape at a glance

```
                       ┌──────────────────────────────────────┐
   HTTP agents ──POST /mcp──►│  axum route (09)  │             │
   (network)                 │   Streamable HTTP │             │
                             └─────────┬─────────┘             │
                                       │  auth ctx (10)        │
   local agents ──stdio──►  3dam mcp ──┤                       │
   (Claude Desktop,         (13)       ▼                       │
    Cursor, editors)          ┌──────────────────┐            │
                              │  McpAdapter        │  rmcp     │
                              │  · tools           │  ServerHandler
                              │  · resources       │            │
                              │  · prompts         │            │
                              └─────────┬──────────┘            │
                                        ▼  in-process trait call │
                              ┌──────────────────────────────────┘
                              │  dyn LibraryService  (03)
                              │  = EmbeddedLibrary (3dam-core)
                              └──►  federation fan-out · auth/scope · non-destructive (for free)
```

Both transports drive **one** `McpAdapter`. The adapter holds a `LibraryService` handle and translates MCP calls to trait calls, in-process — no child process, no stdout scraping. This is the ADR 0003 divergence from MoGen: MoGen shells out because its command functions `exit()`/`println!` and it has no in-process seam to call; 3DAM has exactly that seam (`LibraryService`, returning structured `Result`s and never printing/exiting), so the adapter calls it directly and returns structured content. As a side effect the adapter inherits federation fan-out ([07](07-sources-and-federation.md)), auth/scope ([10](10-auth-accounts-and-flags.md)), and the non-destructive guarantees ([08](08-convert-pipeline.md)) for free — they live below the seam.

---

## 2. Crate & `rmcp` integration

The MCP adapter lives in **`3dam-server`** (alongside the axum router), behind a small module `mcp/`. It depends on `rmcp`, `3dam-api` (the trait + DTOs), and `serde`. It does **not** depend on `3dam-core` directly for its logic — it is written against `dyn LibraryService` so it is transport- and impl-agnostic (in `serve` mode the concrete impl is `EmbeddedLibrary`; the stdio verb constructs the same).

`rmcp` gives us a `ServerHandler` we implement once, and two transport drivers we wire to it:

```rust
// crate: 3dam-server — mcp/mod.rs
use rmcp::{ServerHandler, model::*, service::*};

/// One adapter, two transports. Holds the engine seam + gating config.
pub struct McpAdapter {
    library: Arc<dyn LibraryService>,   // 03 — the only way in
    gate:    WriteGate,                 // §6 — reads always, writes conditional
}

impl McpAdapter {
    pub fn new(library: Arc<dyn LibraryService>, gate: WriteGate) -> Self { … }
}

#[async_trait]
impl ServerHandler for McpAdapter {
    fn get_info(&self) -> ServerInfo {
        ServerInfo {
            capabilities: ServerCapabilities::builder()
                .enable_tools()
                .enable_resources()      // §4 — includes resource templates
                .enable_prompts()        // §5
                .build(),
            server_info: Implementation { name: "3dam".into(), version: crate_version!() },
            instructions: Some(include_str!("instructions.md").into()),
            ..Default::default()
        }
    }

    async fn list_tools(&self, _: Option<PaginatedRequestParam>, cx: RequestContext<RoleServer>)
        -> Result<ListToolsResult, ErrorData> { self.tools_for(cx.auth()).into() }   // §6: writes filtered out when gated

    async fn call_tool(&self, req: CallToolRequestParam, cx: RequestContext<RoleServer>)
        -> Result<CallToolResult, ErrorData> { self.dispatch_tool(req, cx.auth()).await }

    async fn list_resources(…) -> …          // §4
    async fn list_resource_templates(…) -> … // §4
    async fn read_resource(…) -> …           // §4
    async fn list_prompts(…) -> …            // §5
    async fn get_prompt(…) -> …              // §5
}
```

### 2.1 Streamable HTTP transport (primary, `POST /mcp`)

Owned-by-09 mount point; defined here. In `serve` mode the router adds one route:

```rust
// wired in 09's router builder, gated on the tri-state McpServer flag (§7):
// Off ⇒ no route at all; ReadOnly / ReadWrite ⇒ route present, write posture per WriteGate.
if flags.mcp() != McpServer::Off {
    let adapter = McpAdapter::new(library.clone(), WriteGate::from_flags(&flags, &bind));
    router = router.nest_service("/mcp",
        rmcp::transport::streamable_http_server::tower::StreamableHttpService::new(
            move || Ok(adapter.clone()),
            LocalSessionManager::default(),
            Default::default(),
        ));
}
```

- Same bind address, same TLS cert, same auth middleware (10) as the rest of the axum server — the ADR 0003 "one port" requirement. The request passes through 09's auth layer first; the resolved auth context reaches the adapter via `RequestContext`.
- Streamable HTTP means a single `POST /mcp` for request/response, with the server able to upgrade to SSE for streamed/long-running results (e.g. progress on a `scan` or `convert` job — see [08](08-convert-pipeline.md) for the job model, [14](14-concurrency-performance-reliability.md) for cancellation). Session id is carried in the MCP session header; `LocalSessionManager` maps it to per-session state.

### 2.2 stdio transport (`3dam mcp`)

Defined-by-13 verb; its body is here. `3dam mcp` opens an **embedded** engine (no network, no running server, no auth surface) and serves the identical adapter over stdio:

```rust
// invoked by 13's `mcp` subcommand
let library = open_backend(Backend::Embedded { library_path }).await?;  // 01 §4
let adapter = McpAdapter::new(library.into(),
    WriteGate::local_stdio());              // §6: local trust → writes allowed
adapter.serve(rmcp::transport::stdio()).await?;   // stays alive; stdout is JSON-RPC only
```

Because it is the *same* `McpAdapter` over the *same* trait, tool/resource/prompt behaviour is identical to the HTTP path; only transport, auth, and the default write posture differ (§6). This mode is **not** governed by the `McpServer` served-endpoint flag (§7) — it is a separate opt-in CLI invocation.

---

## 3. Tool inventory

Tool **names mirror the CLI verbs** ([13](13-cli.md), PRODUCT_SPEC §6.9) so both surfaces stay learnable together (ADR 0003, borrowed from MoGen). Each tool is a thin translation: parse a typed input struct → call one `LibraryService` method → wrap the DTO as structured tool output. Errors return as `CallToolResult { is_error: true, … }` with a structured payload (ADR 0003), never as a failed RPC.

| Tool | `LibraryService` call | R/W |
|------|-----------------------|-----|
| `search` | `query(SearchQuery)` — text + facets incl. **license facet** ([05](05-analysis-similarity-dedup.md) license model) | R |
| `find_similar` | `find_similar(SimRef)` — by asset id or uploaded reference blob | R |
| `get_asset` | `get_asset(AssetId)` | R |
| `list_sources` | `list_sources()` | R |
| `list_tags` | `list_tags(TagQuery)` | R |
| `find_duplicates` | `find_duplicates(DupQuery)` — exact + near ([05](05-analysis-similarity-dedup.md)) | R |
| `library_stats` | `library_stats()` — counts, media mix, license breakdown | R |
| `tag` / `untag` | `edit_tags(AssetSel, TagDelta)` | **W** |
| `set_license` | `set_license(AssetSel, LicenseDto)` | **W** |
| `add_source` | `add_source(SourceSpec)` — file *or* federated peer ([07](07-sources-and-federation.md)) | **W** |
| `scan` / `rescan` | `submit_scan(ScanSpec)` → job handle | **W** |
| `convert` | `submit_convert(ConvertSpec)` → job handle ([08](08-convert-pipeline.md)) | **W** |
| `export` | `submit_export(ExportSpec)` → manifest/metadata ([08](08-convert-pipeline.md)) | **W** |

Federation is transparent: `search` and `find_similar` fan out across federated peers and merge/re-rank exactly as the HTTP API does ([07](07-sources-and-federation.md)), each hit tagged with its origin peer and carrying its license — the adapter does nothing special, it lives below the seam.

### 3.1 Tool-definition sketch (read tool)

```rust
// input schema is a serde struct → rmcp derives the JSON Schema
#[derive(Deserialize, JsonSchema)]
struct SearchArgs {
    /// Free-text query.
    query: Option<String>,
    /// Facets: media type, tags, source, and the license facet (§5).
    #[serde(default)] license: Option<LicenseFacet>,   // e.g. Unknown | NoCommercial | ...
    #[serde(default)] media:   Option<Vec<MediaKind>>,
    #[serde(default)] tags:    Option<Vec<String>>,
    #[serde(default)] source:  Option<SourceId>,
    #[serde(default)] page:    Option<PageCursor>,
}

async fn tool_search(&self, a: SearchArgs, auth: &AuthContext) -> Result<CallToolResult, ErrorData> {
    let q = SearchQuery::from(a);                       // → 03 DTO
    let page = self.library.query(q).await              // in-process; fans out (07), scoped by auth (10)
        .map_err(structured_tool_error)?;               // Err → is_error result, not RPC failure
    Ok(CallToolResult::structured(json!({               // structured content, plus a short text summary
        "hits":  page.items,
        "next":  page.next_cursor,
        "total": page.total,
    })))
}
```

### 3.2 Tool-definition sketch (write tool, gated + dry-run)

Every write tool takes a `dry_run: bool` (default derived from the gate — §6) and returns a **structured diff** rather than a bare "ok" (PRODUCT_SPEC §6.10, [08](08-convert-pipeline.md) non-destructive policy):

```rust
#[derive(Deserialize, JsonSchema)]
struct ConvertArgs {
    selection: AssetSelector,       // ids or a saved search
    target:    ConvertTarget,       // format/codec/params
    out_dir:   Option<PathBuf>,     // never overwrites a source (08)
    #[serde(default = "default_true")] dry_run: bool,
}

async fn tool_convert(&self, a: ConvertArgs, auth: &AuthContext) -> Result<CallToolResult, ErrorData> {
    self.gate.require_write(Tool::Convert, auth)?;      // §6 — else structured is_error
    let spec = ConvertSpec::from(a).with_dry_run(a.dry_run);
    let plan = self.library.submit_convert(spec).await.map_err(structured_tool_error)?;
    // dry_run → the diff/plan; real run → job handle + the same diff, streamable over SSE (§2.1)
    Ok(CallToolResult::structured(serde_json::to_value(plan)?))
}
```

---

## 4. Resources

Resources make assets and previews **addressable** so an agent can pull them into context without a tool round-trip. Fixed URIs plus **resource templates** for parameterised addressing.

| URI (template) | Content | Backed by |
|----------------|---------|-----------|
| `3dam://asset/{id}` | JSON metadata **including the license/rights block** ([05](05-analysis-similarity-dedup.md)) | `get_asset(id)` |
| `3dam://asset/{id}/preview` | Thumbnail PNG / audio waveform as MCP **image content** ([04](04-media-handlers.md) thumbnail path) | `fetch_preview(id)` |
| `3dam://source/{id}` | Source descriptor (file or federated peer) | `list_sources()` filtered |
| `3dam://saved-search/{name}` | A saved search's current result page | `query(saved)` |
| `3dam://smart-folder/{name}` | A smart-folder's members | `query(rule)` |

```rust
fn resource_templates(&self) -> Vec<ResourceTemplate> {
    vec![
        ResourceTemplate::new("3dam://asset/{id}",           "asset-metadata", "application/json"),
        ResourceTemplate::new("3dam://asset/{id}/preview",   "asset-preview",  "image/png"),
        ResourceTemplate::new("3dam://source/{id}",          "source",         "application/json"),
        ResourceTemplate::new("3dam://saved-search/{name}",  "saved-search",   "application/json"),
        ResourceTemplate::new("3dam://smart-folder/{name}",  "smart-folder",   "application/json"),
    ]
}
```

`read_resource` parses the URI, dispatches to the matching `LibraryService` read call, and returns `TextResourceContents` (JSON) or `BlobResourceContents` / image content (previews). Resources are **read-only by construction** — there is no write resource — and are subject to the same auth/scope as the tools (§6): an anonymous caller reading `3dam://asset/{id}` sees exactly what an anonymous browser would.

---

## 5. Prompts

A small set of canned, multi-step workflows an agent can invoke directly (MCP's third primitive; MoGen exposes none). Each prompt is a named template that expands to guidance + suggested tool/resource calls; it does not itself mutate anything.

| Prompt | Arguments | Expands to |
|--------|-----------|------------|
| `audit-licenses` | `scope?` | Guides `search` with the license facet for **unknown / no-commercial** assets and the **missing-attribution** list; produces a report. |
| `review-duplicates` | `scope?` | Runs `find_duplicates`, groups exact + near, suggests a keep/replace review (non-destructive — [08](08-convert-pipeline.md)). |
| `find-similar-then-export-manifest` | `ref`, `out_dir?` | `find_similar` → refine → `export` a manifest of the chosen set. |

```rust
fn prompts(&self) -> Vec<Prompt> {
    vec![
        Prompt::new("audit-licenses",  "Surface unknown / no-commercial assets and missing attributions.",
                    &[arg("scope", false)]),
        Prompt::new("review-duplicates", "Group exact + near duplicates for a keep/replace review.",
                    &[arg("scope", false)]),
        Prompt::new("find-similar-then-export-manifest",
                    "Find similar to a reference, refine, and export a manifest.",
                    &[arg("ref", true), arg("out_dir", false)]),
    ]
}
```

`get_prompt` fills the template with the caller's arguments and returns the message sequence. Prompts that suggest write tools (e.g. `export`) still run those tools through the gate (§6) — the prompt is convenience, not a bypass.

---

## 6. Safety & write-gating

The rule (PRODUCT_SPEC §6.10, ADR 0003 §Consequences): **reads are always available; writes are conditional.** The decision is made per call by `WriteGate`, which combines *where the server is bound*, *the flag*, and *the caller's auth scope* ([10](10-auth-accounts-and-flags.md)).

```
call_tool(tool)
   │
   ├─ tool is READ ───────────────────────────────► allow  (scoped by auth — 10 filters results)
   │
   └─ tool is WRITE
        │
        ├─ transport == stdio (local, embedded) ──► allow   (WriteGate::local_stdio)
        │
        └─ transport == HTTP
              │
              ├─ bind is localhost-only ──┐
              │                           ├─ flag == McpServer::ReadWrite? ─┬─ no ─► deny (is_error: writes disabled)
              │  bind beyond localhost ───┘                                └─ yes ─┐
              │      (default = READ-ONLY unless flag is ReadWrite)                │
              │                                                                ▼
              └──────────────────────────────► auth scope has `write`? ─┬─ no ─► deny (is_error: forbidden)
                                                                         └─ yes ─► allow (honours dry-run + non-destructive, §3.2)
```

- **Read-only default beyond localhost — for implicit trust.** The `McpServer` flag is the surface gate: `ReadWrite` exposes write tools, anything less hides them from everyone. Past that, a **verified** caller (token identity) needs only the `write` scope; an **implicit-trust** caller (anonymous, or the auth-off owner posture) on a non-loopback bind additionally needs `NetworkWrites` — the same ceiling-on-implicit-trust rule as the REST write handlers ([10](10-auth-accounts-and-flags.md) §4.2). An anonymous caller sees only the read tools — the same posture as an anonymous browser.
- **Non-destructive always.** Allowed writes still honour [08](08-convert-pipeline.md): never overwrite a source silently, outputs go to a chosen location, expensive/mutating tools support **dry-run** and return **structured diffs** (§3.2).
- **Scope, not just a flag.** The flag opens the *possibility* of writes; per-call auth scope decides each one. Both must pass. `WriteGate` reads the flag snapshot ([10](10-auth-accounts-and-flags.md) flag store) and the `AuthContext` from `RequestContext`.
- **stdio is locally trusted.** `3dam mcp` runs against an embedded engine the user already controls; `WriteGate::local_stdio()` allows writes with no auth surface. (It is still non-destructive — the gate governs *authorisation*, [08](08-convert-pipeline.md) governs *destructiveness*.)

---

## 7. Switchable off (served endpoint only)

The served MCP endpoint is the tri-state **feature flag** `McpServer` (`Off` | `ReadOnly` | `ReadWrite`), defined in [10](10-auth-accounts-and-flags.md). `Off` means the route and its tools are **removed**, not dormant behind auth (PRODUCT_SPEC §6.10, [ADR 0004](../adr/0004-feature-flags-admin.md)):

- When the flag is `Off`, 09's router builder **does not add the `/mcp` route** (§2.1) — it is unmounted (404, never 403). There is no endpoint to probe, no tool list to enumerate — an operator who does not want agent access carries none of its surface.
- `ReadOnly` mounts the route but does not expose the write tools; `ReadWrite` exposes them (each call still needs the `Write` scope, and implicit-trust callers beyond localhost also need the `NetworkWrites` flag, §6).
- Flag lifecycle (live-toggle vs restart) is owned by [10](10-auth-accounts-and-flags.md); this file only requires that flipping the flag adds/removes the route accordingly. If the flag is live-toggleable, 09 rebuilds/guards the route; if restart-scoped, it takes effect on next `serve`.
- The config-file `[mcp] enabled=…, writes=…` block is the file-level spelling that **seeds** this flag ([10](10-auth-accounts-and-flags.md) owns reconciliation) — config-surface vs stored-flag, not a contradiction.
- The **stdio `3dam mcp` mode is unaffected** by the `McpServer` flag: it is a separate opt-in CLI invocation over an embedded engine, with no served endpoint to remove ([13](13-cli.md)).

---

## Open questions

> **Resolved 2026-07-06 in [ADR 0009 §6](../adr/0009-v1-scope-decisions.md).** A small purpose-tool
> set + resources, read-only by default, per-tool opt-in writes gated on auth beyond localhost, and
> **no transitive peer MCP.** Verb↔tool parity: keep each surface idiomatic, back both with one verb
> registry + an explicit name map (§3). Kept below as rationale.

- **MCP surface & safety** (carried from PRODUCT_SPEC §10): tool granularity (one broad `search` vs many narrow tools), how far to lean on resources/prompts vs tools, and exactly which write tools to expose and how they are gated when the server is exposed beyond localhost (read-only default, auth scopes, per-tool opt-in). Also: whether federated peers' MCP endpoints should be reachable **transitively** through this server, or only their catalogs via this server's own fan-out tools (ADR 0003 §Follow-ups).
- **CLI-verb vs MCP-tool-name parity.** The MCP tool names (`find_similar`, `add_source`) and the CLI verbs they mirror (`similar`, `source add`) have drifted; §3 says tool names mirror the CLI verbs, but the two surfaces are not yet 1:1. Whether to unify the spellings or keep each surface ergonomically idiomatic is unresolved (shared with [13](13-cli.md)).
