# ADR 0003 — MCP server: one port, in-process over `LibraryService`

Status: **Accepted (draft)** · Date: 2026-07-06 · Deciders: 3DAM core
Supersedes: — · Related: [PRODUCT_SPEC.md](../PRODUCT_SPEC.md) §6.8, §6.10, §6.11, §7,
[0002 — 3D render crate boundary](0002-3d-render-crate-boundary.md),
[0004 — feature flags & server administration](0004-feature-flags-admin.md)

## Context

3DAM must be drivable by LLM agents, not just people — search, inspect, tag, convert, and
audit a library through the Model Context Protocol (MCP). The sibling **MoGen** project
(`../godot-projects/mogen`) already ships this and is our reference: `mogen mcp`
(`crates/mogen/src/commands/mcp.rs`) is a **stdio** server, built on the `rmcp` crate, that
exposes **every CLI subcommand as a tool**. Each tool re-invokes the `mogen` binary as a
**subprocess** and captures its stdout/stderr.

MoGen's subprocess design is a workaround for *its* code shape, spelled out in that file's
header: several commands call `std::process::exit(1)`, and many `println!` results to
stdout — both fatal inside a stdio MCP server, where the process must stay alive and stdout
is reserved for JSON-RPC. Going via a child process sidesteps both.

3DAM's shape is different in two ways that change the right answer:

1. **There is already one clean seam.** Every client talks to the engine through a single
   `LibraryService` trait ([PRODUCT_SPEC.md](../PRODUCT_SPEC.md) §4.2), which returns
   structured values and never `exit()`s or prints. MoGen has no such in-process boundary to
   call, which is *why* it shells out.
2. **The server already owns a port.** `3dam serve` runs an async HTTP/WebSocket server and
   hosts the web client (§6.8). The user requirement is explicit: the agent interface should
   live **under one port**, not as a second daemon.

Two decisions follow: how tools reach the engine, and what transport(s) carry MCP.

## Decision

**1. MCP is a frontend over `LibraryService`, in-process — not a subprocess wrapper over the
CLI.** MCP tools call the engine directly through the same `LibraryService` boundary the
HTTP API and web client use, and return structured results. No child process, no stdout
scraping. This is a deliberate divergence from MoGen, justified by 3DAM having the
in-process seam MoGen lacks.

**2. The primary transport is MCP Streamable HTTP, mounted on the `serve` port.** In `serve`
mode the MCP endpoint (e.g. `POST /mcp`) is another route on the same `axum` server as the
HTTP/WS API and the web client — one bind address, one auth surface (§6.7), one TLS cert,
one firewall rule. This satisfies "everything under one port."

**3. Also offer `3dam mcp` over stdio.** For local agents (Claude Desktop, Cursor, editors)
that spawn the binary directly against an embedded engine with no network, `3dam mcp` runs
the identical tool surface over stdio — the mode MoGen ships, kept as a secondary path.

**4. Use `rmcp`** (the Rust MCP SDK MoGen already validates) for both transports, and expose
all three MCP primitives — **tools, resources, and prompts** — where MoGen exposes only
tools (§6.10).

What we **borrow from MoGen**: the "whole surface, reachable by an agent" instinct; tool
names that mirror the CLI verbs so both surfaces stay learnable together; returning tool
errors as structured `is_error` results rather than failing the RPC; and `rmcp` itself.

## Consequences

**Positive**

- One port and one auth/TLS surface for API, web client, federated peers, and agents —
  matches the self-hostable posture and the user's explicit constraint.
- Tools inherit federation fan-out, auth/scope, and the non-destructive guarantees (§8) for
  free, because they sit on the same `LibraryService` every other client uses.
- Structured, in-process results — no subprocess spawn per call, no stdout parsing, no risk
  of a stray `println!` corrupting the protocol.

**Negative / risks**

- The engine functions behind `LibraryService` must be genuinely library-safe (no `exit()`,
  errors as `Result`) — an in-process MCP server has no subprocess to absorb a rogue exit.
  This is a constraint MoGen avoided by shelling out; we take it on deliberately and it is
  already required by the embedded-engine role.
- **Write tools are a real exposure.** `tag` / `set_license` / `add_source` / `scan` /
  `convert` / `export` mutate state, so the HTTP surface must default to **read-only** when
  bound beyond localhost, gate writes behind explicit config + auth scope, and support
  dry-run (§6.10). Tracked as an open question in §10.
- Streamable HTTP + auth + optional SSE streaming is more protocol surface than MoGen's plain
  stdio; the stdio mode stays as the simple, no-auth local fallback.

**Follow-ups**

- Settle tool granularity (one broad `search` vs many narrow tools) and the tools-vs-resources
  split (§10).
- Decide whether an agent may reach federated peers' MCP endpoints transitively, or only the
  peers' catalogs via this server's own tools (§10).
