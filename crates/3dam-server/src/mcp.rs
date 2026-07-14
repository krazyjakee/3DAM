//! The MCP server (tech-spec 11, ADR 0003) — one adapter, two transports.
//!
//! [`McpAdapter`] holds a `LibraryService` handle and translates MCP calls into trait calls
//! **in-process** — no child process, no stdout scraping (the ADR 0003 divergence from MoGen).
//! Because the trait returns structured `Result`s and never prints/exits, the adapter can call it
//! directly and return structured content, inheriting federation, auth/scope, and the
//! non-destructive guarantees for free (they live below the seam).
//!
//! The wire protocol is MCP's JSON-RPC 2.0. It is implemented directly here (rather than pulling in
//! `rmcp`) for the v1 request/response surface — matching this codebase's hand-rolled,
//! dependency-light handlers — over both **stdio** (`3dam mcp`, locally trusted) and the
//! Streamable-HTTP `POST /mcp` route (mounted on the shared serve port, behind the same auth layer).
//! Tool names mirror the CLI verbs (ADR 0009 §6). Reads are always available; writes are gated by
//! the [`WriteGate`] (bind + `McpServer` flag + caller scope, tech-spec 11 §6).

use dam_api::admin::McpMode;
use dam_api::dto::*;
use dam_api::id::AssetId;
use dam_api::page::PageParams;
use dam_api::service::{AuthContext, LibraryService, Scope};
use dam_api::LibError;
use serde_json::{json, Value};
use std::sync::Arc;

/// The MCP protocol revision we speak.
pub const MCP_PROTOCOL_VERSION: &str = "2025-06-18";

/// Decides whether a write tool may run (tech-spec 11 §6). Combines *where the server is bound*, the
/// `McpServer` flag, and (per call) the caller's `Write` scope. stdio is locally trusted.
#[derive(Clone, Copy)]
pub struct WriteGate {
    allow: bool,
}

impl WriteGate {
    /// stdio runs against an embedded engine the user already controls → writes allowed (still
    /// non-destructive; the gate governs authorisation, not destructiveness — tech-spec 11 §6).
    pub fn local_stdio() -> Self {
        WriteGate { allow: true }
    }
    /// The served endpoint: writes need `McpServer = ReadWrite` *and* an allowed bind (localhost, or
    /// the `NetworkWrites` flag when bound beyond localhost).
    pub fn from_flags(mcp: McpMode, localhost_only: bool, network_writes: bool) -> Self {
        let allow = matches!(mcp, McpMode::ReadWrite) && (localhost_only || network_writes);
        WriteGate { allow }
    }
    /// A write is permitted iff the transport/flag posture allows it and the caller holds `Write`.
    fn permits(&self, ctx: &AuthContext) -> bool {
        self.allow && (ctx.embedded || ctx.scopes.has(Scope::Write))
    }
}

/// A tool descriptor: its name, one-line description, whether it mutates, and its input schema.
struct ToolDef {
    name: &'static str,
    description: &'static str,
    write: bool,
    schema: fn() -> Value,
}

fn tool_defs() -> Vec<ToolDef> {
    vec![
        ToolDef {
            name: "search",
            description: "Search the library by text and facets (media, tags, source).",
            write: false,
            schema: || {
                json!({
                    "type": "object",
                    "properties": {
                        "query": {"type": "string", "description": "Free text over filename."},
                        "media": {"type": "string", "enum": ["audio","image","model"]},
                        "tags": {"type": "array", "items": {"type": "string"}},
                        "source": {"type": "string", "description": "Source id (UUID)."},
                        "limit": {"type": "integer", "minimum": 1, "maximum": 200, "default": 50}
                    }
                })
            },
        },
        ToolDef {
            name: "find_similar",
            description: "Nearest neighbours of an asset by embedding cosine (\"more like this\").",
            write: false,
            schema: || {
                json!({
                    "type": "object",
                    "required": ["id"],
                    "properties": {
                        "id": {"type": "string", "description": "The query asset id (UUID)."},
                        "limit": {"type": "integer", "minimum": 1, "maximum": 100, "default": 12}
                    }
                })
            },
        },
        ToolDef {
            name: "get_asset",
            description: "Full metadata for one asset, including its license block.",
            write: false,
            schema: || {
                json!({
                    "type": "object", "required": ["id"],
                    "properties": {"id": {"type": "string", "description": "Asset id (UUID)."}}
                })
            },
        },
        ToolDef {
            name: "list_sources",
            description: "List configured sources (file + federated).",
            write: false,
            schema: || json!({"type": "object", "properties": {}}),
        },
        ToolDef {
            name: "library_stats",
            description: "Library counts, media mix, and unanalysed backlog.",
            write: false,
            schema: || json!({"type": "object", "properties": {}}),
        },
        ToolDef {
            name: "find_duplicates",
            description:
                "Duplicate groups for review (exact or near). Grouping only — never deletes.",
            write: false,
            schema: || {
                json!({
                    "type": "object",
                    "properties": {
                        "near": {"type": "boolean", "default": false},
                        "media": {"type": "string", "enum": ["audio","image","model"]},
                        "limit": {"type": "integer", "minimum": 1, "maximum": 200, "default": 50}
                    }
                })
            },
        },
        // ── write tools (gated, §6) ──────────────────────────────────────────
        ToolDef {
            name: "tag",
            description: "Accept (or --reject) an auto-suggested tag on an asset. Reversible.",
            write: true,
            schema: || {
                json!({
                    "type": "object", "required": ["id","tag"],
                    "properties": {
                        "id": {"type": "string"}, "tag": {"type": "string"},
                        "reject": {"type": "boolean", "default": false}
                    }
                })
            },
        },
        ToolDef {
            name: "add_source",
            description: "Register a source (local path or network peer).",
            write: true,
            schema: || {
                json!({
                    "type": "object", "required": ["kind","uri"],
                    "properties": {
                        "kind": {"type": "string", "enum": ["local_fs","sftp","smb","federated"]},
                        "uri": {"type": "string"}, "name": {"type": "string"}
                    }
                })
            },
        },
        ToolDef {
            name: "scan",
            description: "Scan sources for new/changed assets. Returns a job id.",
            write: true,
            schema: || {
                json!({
                    "type": "object",
                    "properties": {"sources": {"type": "array", "items": {"type": "string"}}}
                })
            },
        },
        ToolDef {
            name: "convert",
            description:
                "Non-destructively convert assets into an output directory. Supports dry_run.",
            write: true,
            schema: || {
                json!({
                    "type": "object", "required": ["inputs","target","output_dir"],
                    "properties": {
                        "inputs": {"type": "array", "items": {"type": "string"}},
                        "target": {"type": "object"},
                        "output_dir": {"type": "string"},
                        "dry_run": {"type": "boolean", "default": true}
                    }
                })
            },
        },
        ToolDef {
            name: "export",
            description:
                "Export a manifest (json/csv/sidecar) of assets, a collection, or a search.",
            write: true,
            schema: || {
                json!({
                    "type": "object", "required": ["format","output"],
                    "properties": {
                        "assets": {"type": "array", "items": {"type": "string"}},
                        "format": {"type": "string", "enum": ["json","csv","sidecar"]},
                        "output": {"type": "string"},
                        "attribution_only": {"type": "boolean", "default": false}
                    }
                })
            },
        },
    ]
}

/// One adapter, two transports (tech-spec 11 §1). Cheap to clone (it is just two `Arc`s of handles).
#[derive(Clone)]
pub struct McpAdapter {
    library: Arc<dyn LibraryService>,
    gate: WriteGate,
}

impl McpAdapter {
    pub fn new(library: Arc<dyn LibraryService>, gate: WriteGate) -> Self {
        McpAdapter { library, gate }
    }

    /// Serve MCP over stdio until EOF (`3dam mcp`). Newline-delimited JSON-RPC; stdout carries only
    /// protocol messages (tech-spec 11 §2.2). Runs against the given (locally-trusted) context.
    pub async fn serve_stdio(self, ctx: AuthContext) -> anyhow::Result<()> {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
        let mut lines = BufReader::new(tokio::io::stdin()).lines();
        let mut out = tokio::io::stdout();
        while let Some(line) = lines.next_line().await? {
            if line.trim().is_empty() {
                continue;
            }
            let reply = match serde_json::from_str::<Value>(&line) {
                Ok(msg) => self.handle_message(&ctx, msg).await,
                Err(e) => Some(rpc_error(Value::Null, -32700, &format!("parse error: {e}"))),
            };
            if let Some(resp) = reply {
                let s = serde_json::to_string(&resp)?;
                out.write_all(s.as_bytes()).await?;
                out.write_all(b"\n").await?;
                out.flush().await?;
            }
        }
        Ok(())
    }

    /// Handle one JSON-RPC message. Returns `Some(response)` for a request, `None` for a
    /// notification (no `id`). Tool/handler errors come back as `isError` results, not RPC failures
    /// (ADR 0003); only protocol-level problems (unknown method, bad params) are RPC errors.
    pub async fn handle_message(&self, ctx: &AuthContext, msg: Value) -> Option<Value> {
        let id = msg.get("id").cloned();
        let method = msg.get("method").and_then(|m| m.as_str()).unwrap_or("");
        let params = msg.get("params").cloned().unwrap_or(Value::Null);
        // A message without an `id` is a notification: process (nothing to do for ours) and stay silent.
        let is_notification = id.is_none();

        let result = self.dispatch(ctx, method, params).await;
        if is_notification {
            return None;
        }
        let id = id.unwrap_or(Value::Null);
        match result {
            Ok(value) => Some(json!({"jsonrpc": "2.0", "id": id, "result": value})),
            Err(RpcError { code, message }) => Some(rpc_error(id, code, &message)),
        }
    }

    async fn dispatch(
        &self,
        ctx: &AuthContext,
        method: &str,
        params: Value,
    ) -> Result<Value, RpcError> {
        match method {
            "initialize" => Ok(json!({
                "protocolVersion": MCP_PROTOCOL_VERSION,
                "capabilities": {"tools": {}, "resources": {}, "prompts": {}},
                "serverInfo": {"name": "3dam", "version": env!("CARGO_PKG_VERSION")},
                "instructions": INSTRUCTIONS,
            })),
            "notifications/initialized" | "notifications/cancelled" => Ok(Value::Null),
            "ping" => Ok(json!({})),
            "tools/list" => Ok(json!({ "tools": self.list_tools(ctx) })),
            "tools/call" => Ok(self.call_tool(ctx, params).await),
            "resources/list" => Ok(json!({ "resources": [] })),
            "resources/templates/list" => Ok(json!({ "resourceTemplates": resource_templates() })),
            "resources/read" => self.read_resource(ctx, params).await,
            "prompts/list" => Ok(json!({ "prompts": prompt_defs() })),
            "prompts/get" => get_prompt(params),
            other => Err(RpcError {
                code: -32601,
                message: format!("method not found: {other}"),
            }),
        }
    }

    /// Tool list, filtered to what the caller may actually run: write tools are omitted unless the
    /// gate permits them (tech-spec 11 §6 — an anonymous/read-only caller sees only read tools).
    fn list_tools(&self, ctx: &AuthContext) -> Vec<Value> {
        tool_defs()
            .into_iter()
            .filter(|t| !t.write || self.gate.permits(ctx))
            .map(|t| {
                json!({
                    "name": t.name,
                    "description": t.description,
                    "inputSchema": (t.schema)(),
                })
            })
            .collect()
    }

    async fn call_tool(&self, ctx: &AuthContext, params: Value) -> Value {
        let name = params.get("name").and_then(|n| n.as_str()).unwrap_or("");
        let args = params.get("arguments").cloned().unwrap_or(json!({}));
        let def = tool_defs().into_iter().find(|t| t.name == name);
        let Some(def) = def else {
            return tool_error(&format!("unknown tool: {name}"));
        };
        if def.write && !self.gate.permits(ctx) {
            return tool_error(
                "write tools are disabled on this server (MCP is read-only or the caller lacks the write scope)",
            );
        }
        match self.run_tool(ctx, name, args).await {
            Ok(value) => tool_ok(value),
            Err(e) => tool_error(&e.to_string()),
        }
    }

    async fn run_tool(
        &self,
        ctx: &AuthContext,
        name: &str,
        args: Value,
    ) -> Result<Value, LibError> {
        let lib = &self.library;
        match name {
            "search" => {
                let a: SearchArgs = parse_args(args)?;
                let mut filters = Vec::new();
                if let Some(m) = a.media {
                    filters.push(eq_filter(FacetField::MediaType, m));
                }
                if let Some(s) = a.source {
                    filters.push(eq_filter(FacetField::Source, s));
                }
                for t in a.tags.unwrap_or_default() {
                    filters.push(eq_filter(FacetField::Tag, t));
                }
                let req = QueryRequest {
                    text: a.query,
                    filters,
                    page: PageParams {
                        after: None,
                        limit: a.limit.unwrap_or(50),
                    },
                    ..Default::default()
                };
                let page = lib.query(ctx, req).await?;
                to_value(&page)
            }
            "find_similar" => {
                let a: IdLimitArgs = parse_args(args)?;
                let req = SimilarRequest {
                    asset: parse_asset(&a.id)?,
                    k: a.limit.unwrap_or(12),
                    filters: Vec::new(),
                    local_only: false,
                };
                to_value(&lib.find_similar(ctx, req).await?)
            }
            "get_asset" => {
                let a: IdArgs = parse_args(args)?;
                to_value(&lib.get_asset(ctx, &parse_asset(&a.id)?).await?)
            }
            "list_sources" => to_value(&lib.list_sources(ctx).await?),
            "library_stats" => to_value(&lib.library_stats(ctx, None).await?),
            "find_duplicates" => {
                let a: DupArgs = parse_args(args)?;
                let media = match a.media.as_deref() {
                    Some(m) => Some(
                        MediaType::parse(m)
                            .ok_or_else(|| LibError::BadRequest(format!("invalid media '{m}'")))?,
                    ),
                    None => None,
                };
                let req = DupRequest {
                    kind: if a.near {
                        DupKind::Near
                    } else {
                        DupKind::Exact
                    },
                    media,
                    limit: a.limit.unwrap_or(50),
                };
                to_value(&lib.list_duplicates(ctx, req).await?)
            }
            "tag" => {
                let a: TagArgs = parse_args(args)?;
                let req = SuggestionReview {
                    asset: parse_asset(&a.id)?,
                    tag: a.tag,
                    action: if a.reject {
                        ReviewAction::Reject
                    } else {
                        ReviewAction::Accept
                    },
                };
                lib.review_suggestion(ctx, req).await?;
                Ok(json!({"ok": true}))
            }
            "add_source" => {
                let req: AddSource = parse_args(args)?;
                let id = lib.add_source(ctx, req).await?;
                Ok(json!({"id": id}))
            }
            "scan" => {
                let a: ScanArgs = parse_args(args)?;
                let sources = a
                    .sources
                    .unwrap_or_default()
                    .iter()
                    .map(|s| {
                        s.parse()
                            .map_err(|_| LibError::BadRequest(format!("bad source id: {s}")))
                    })
                    .collect::<Result<Vec<_>, _>>()?;
                let job = lib
                    .submit_scan(
                        ctx,
                        ScanRequest {
                            sources,
                            mode: ScanMode::Full,
                        },
                    )
                    .await?;
                Ok(json!({"job_id": job}))
            }
            "convert" => {
                let req: ConvertRequest = parse_args(args)?;
                to_value(&lib.convert(ctx, req).await?)
            }
            "export" => {
                let req: ExportRequest = parse_args(args)?;
                to_value(&lib.export(ctx, req).await?)
            }
            other => Err(LibError::NotFound(format!("unknown tool: {other}"))),
        }
    }

    async fn read_resource(&self, ctx: &AuthContext, params: Value) -> Result<Value, RpcError> {
        let uri = params
            .get("uri")
            .and_then(|u| u.as_str())
            .ok_or_else(|| RpcError {
                code: -32602,
                message: "missing uri".into(),
            })?
            .to_string();
        match self.resolve_resource(ctx, &uri).await {
            Ok(contents) => Ok(json!({ "contents": [contents] })),
            Err(e) => Err(RpcError {
                code: -32002,
                message: e.to_string(),
            }),
        }
    }

    async fn resolve_resource(&self, ctx: &AuthContext, uri: &str) -> Result<Value, LibError> {
        let rest = uri
            .strip_prefix("3dam://")
            .ok_or_else(|| LibError::BadRequest(format!("not a 3dam uri: {uri}")))?;
        let parts: Vec<&str> = rest.split('/').collect();
        match parts.as_slice() {
            ["asset", id, "preview"] => {
                let thumb = self
                    .library
                    .read_thumbnail(ctx, &parse_asset(id)?, 256)
                    .await?;
                Ok(json!({
                    "uri": uri,
                    "mimeType": thumb.content_type,
                    "blob": base64_encode(&thumb.bytes),
                }))
            }
            ["asset", id] => {
                let asset = self.library.get_asset(ctx, &parse_asset(id)?).await?;
                Ok(text_resource(
                    uri,
                    "application/json",
                    &serde_json::to_string(&asset).unwrap(),
                ))
            }
            ["source", id] => {
                let sid = id
                    .parse()
                    .map_err(|_| LibError::BadRequest("bad source id".into()))?;
                let src = self.library.get_source(ctx, &sid).await?;
                Ok(text_resource(
                    uri,
                    "application/json",
                    &serde_json::to_string(&src).unwrap(),
                ))
            }
            ["collection", id] => {
                let cid = id
                    .parse()
                    .map_err(|_| LibError::BadRequest("bad collection id".into()))?;
                let col = self.library.get_collection(ctx, &cid).await?;
                Ok(text_resource(
                    uri,
                    "application/json",
                    &serde_json::to_string(&col).unwrap(),
                ))
            }
            _ => Err(LibError::NotFound(format!("no resource: {uri}"))),
        }
    }
}

// ── tool argument shapes (agent-friendly; built into the real request DTOs) ──────────────────────

#[derive(serde::Deserialize)]
struct SearchArgs {
    query: Option<String>,
    media: Option<String>,
    tags: Option<Vec<String>>,
    source: Option<String>,
    limit: Option<u32>,
}
#[derive(serde::Deserialize)]
struct IdArgs {
    id: String,
}
#[derive(serde::Deserialize)]
struct IdLimitArgs {
    id: String,
    limit: Option<u32>,
}
#[derive(serde::Deserialize)]
struct DupArgs {
    #[serde(default)]
    near: bool,
    media: Option<String>,
    limit: Option<u32>,
}
#[derive(serde::Deserialize)]
struct TagArgs {
    id: String,
    tag: String,
    #[serde(default)]
    reject: bool,
}
#[derive(serde::Deserialize)]
struct ScanArgs {
    sources: Option<Vec<String>>,
}

fn parse_args<T: serde::de::DeserializeOwned>(v: Value) -> Result<T, LibError> {
    serde_json::from_value(v).map_err(|e| LibError::BadRequest(format!("bad tool arguments: {e}")))
}
fn parse_asset(s: &str) -> Result<AssetId, LibError> {
    s.parse()
        .map_err(|_| LibError::BadRequest(format!("bad asset id: {s}")))
}
fn eq_filter(field: FacetField, value: String) -> Filter {
    Filter {
        field,
        op: FilterOp::Eq,
        value: FilterValue::Str(value),
    }
}
fn to_value<T: serde::Serialize>(v: &T) -> Result<Value, LibError> {
    serde_json::to_value(v).map_err(|e| LibError::Internal(e.to_string()))
}

// ── MCP result envelopes ─────────────────────────────────────────────────────────────────────────

/// A successful tool result: structured content plus a short text rendering (tech-spec 11 §3.1).
fn tool_ok(value: Value) -> Value {
    let text = serde_json::to_string_pretty(&value).unwrap_or_else(|_| value.to_string());
    json!({
        "content": [{"type": "text", "text": text}],
        "structuredContent": value,
        "isError": false,
    })
}

/// A tool error — returned as an `isError` result, never a failed RPC (ADR 0003).
fn tool_error(message: &str) -> Value {
    json!({
        "content": [{"type": "text", "text": format!("error: {message}")}],
        "isError": true,
    })
}

fn text_resource(uri: &str, mime: &str, text: &str) -> Value {
    json!({"uri": uri, "mimeType": mime, "text": text})
}

struct RpcError {
    code: i64,
    message: String,
}

fn rpc_error(id: Value, code: i64, message: &str) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}})
}

fn resource_templates() -> Value {
    json!([
        {"uriTemplate": "3dam://asset/{id}", "name": "asset-metadata", "mimeType": "application/json"},
        {"uriTemplate": "3dam://asset/{id}/preview", "name": "asset-preview", "mimeType": "image/png"},
        {"uriTemplate": "3dam://source/{id}", "name": "source", "mimeType": "application/json"},
        {"uriTemplate": "3dam://collection/{id}", "name": "collection", "mimeType": "application/json"}
    ])
}

fn prompt_defs() -> Value {
    json!([
        {"name": "audit-licenses", "description": "Surface unknown / no-commercial assets and missing attributions.",
         "arguments": [{"name": "scope", "required": false}]},
        {"name": "review-duplicates", "description": "Group exact + near duplicates for a keep/replace review.",
         "arguments": [{"name": "scope", "required": false}]},
        {"name": "find-similar-then-export-manifest", "description": "Find similar to a reference, refine, and export a manifest.",
         "arguments": [{"name": "ref", "required": true}, {"name": "out_dir", "required": false}]}
    ])
}

fn get_prompt(params: Value) -> Result<Value, RpcError> {
    let name = params.get("name").and_then(|n| n.as_str()).unwrap_or("");
    let text = match name {
        "audit-licenses" => "Use `search` with a license facet to list assets whose license is unknown or non-commercial, and flag any missing attribution. Produce a short report grouped by license status.",
        "review-duplicates" => "Call `find_duplicates` for exact groups, then again with near=true. For each group, suggest which member to keep (largest / best format) and which to replace. Do not delete — this is review only.",
        "find-similar-then-export-manifest" => "Call `find_similar` on the reference asset, refine the set with the caller's constraints, then `export` a json manifest of the chosen ids to the given output directory.",
        other => return Err(RpcError { code: -32602, message: format!("unknown prompt: {other}") }),
    };
    Ok(json!({
        "description": "3dam workflow prompt",
        "messages": [{"role": "user", "content": {"type": "text", "text": text}}],
    }))
}

const INSTRUCTIONS: &str = "3dam is a local-first game-asset manager (audio, images, 3D models). \
Use `search` and `find_similar` to explore the catalog, `get_asset` for full metadata including \
license, and `find_duplicates` for dedup review. Write tools (tag/add_source/scan/convert/export) \
are non-destructive and only available when the server enables them.";

/// Minimal standard-alphabet base64 (no padding omitted) for MCP blob resource contents — avoids a
/// dependency for the one place we hand back binary preview bytes.
fn base64_encode(data: &[u8]) -> String {
    const A: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | (b[2] as u32);
        out.push(A[((n >> 18) & 63) as usize] as char);
        out.push(A[((n >> 12) & 63) as usize] as char);
        out.push(if chunk.len() > 1 {
            A[((n >> 6) & 63) as usize] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            A[(n & 63) as usize] as char
        } else {
            '='
        });
    }
    out
}
