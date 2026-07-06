# 13 — CLI

Status: **Draft v0.1** · Scope: the `3dam` CLI — the clap command tree, global flags, human vs `--json`/`--csv` output, the exit-code table, `--dry-run`/`--yes` no-surprises rules, `--connect`, and how the `serve`/`mcp` subcommands are dispatched from the same binary.

This file is the low-level companion to [PRODUCT_SPEC.md](../PRODUCT_SPEC.md) §6.9 (CLI), §4.1 (one binary, three roles), and §6.11 (administer flags/accounts headlessly), and to [DESIGN_GUIDELINES.md](../DESIGN_GUIDELINES.md) §5 (CLI guidelines) and §1.4. Those establish *what* the CLI is — a first-class, scriptable front-end with full parity, verb-noun commands, machine output on request, meaningful exit codes, no required prompts in non-interactive mode. This file turns that into a concrete `clap` grammar, an exit-code table CI can branch on, and worked invocations with their JSON shapes.

It does **not** own the pieces it drives. The CLI is a thin front-end that lives in the `3dam-cli` crate ([01-architecture-and-crates.md](01-architecture-and-crates.md) §2) and reaches the engine only through the `LibraryService` seam ([03-library-service-and-api.md](03-library-service-and-api.md)) — it invents no capability the service does not already expose. `serve` internals belong to [09-server-and-web-client.md](09-server-and-web-client.md), MCP internals to [11-mcp-server.md](11-mcp-server.md), convert internals to [08-convert-pipeline.md](08-convert-pipeline.md), and flag/account/auth *semantics* to [10-auth-accounts-and-flags.md](10-auth-accounts-and-flags.md); this file owns only their **command surface** (verbs, flags, output). Role dispatch — how a bare `3dam`, a verb, `serve`, and `mcp` fan out from one binary — is fixed in [01-architecture-and-crates.md](01-architecture-and-crates.md) §5 and only referenced here.

---

## 1. The shape of a command

Every invocation is `3dam [GLOBAL FLAGS] <verb> [SUBVERB] [ARGS] [VERB FLAGS]`. Verbs are the parity surface: each maps onto one or a small composition of `LibraryService` calls (§4). Two verbs — `serve` and `mcp` — are grammatically part of the same clap tree but dispatch out of ordinary CLI handling into long-lived services ([01](01-architecture-and-crates.md) §5); the rest are run-and-exit.

The design obeys four house rules from DESIGN_GUIDELINES §5, wired concretely below:

- **Verb-noun** (§5): `3dam scan <source>`, `3dam set-license <asset> <spdx>`, `3dam source add …`.
- **Human by default, machine on request** (§5): TTY → tables; `--json`/`--csv` or a non-TTY stdout → machine output (§5, §6 below).
- **Composable** (§5): results **stream** to stdout as they arrive; exit codes are meaningful (§7); output pipes into `jq`/`csvkit`/the next `3dam` call.
- **No surprises** (§5): anything that writes supports `--dry-run` and, when destructive or writing to a remote/network target, requires `--yes` (or an interactive confirm) — §8.

---

## 2. Global flags

Parsed by clap at the top level, before the verb, and available to every verb (clap `global = true`). They select the **backend**, the **output mode**, and the **safety posture**.

| Flag | Type | Default | Effect |
|------|------|---------|--------|
| `--connect <host:port>` | string | *(unset → embedded)* | Use a remote `3dam serve` as the backend over its HTTP/WS API instead of the embedded engine ([01](01-architecture-and-crates.md) §4; PRODUCT_SPEC §6.8). Selects the `ApiClient` `LibraryService`. |
| `--config <path>` | path | platform config dir | Path to the shared config/DB profile ([15](15-observability-config-testing-packaging.md)); the *same* file the desktop GUI uses (DESIGN_GUIDELINES §5, "shared config and database"). |
| `--json` | flag | off | Emit machine output as JSON (one document, or NDJSON when streaming — §6). Mutually exclusive with `--csv`. |
| `--csv` | flag | off | Emit machine output as CSV (header row + rows). Mutually exclusive with `--json`. |
| `--dry-run` | flag | off | For writing verbs: compute and report the effect, mutate nothing (§8). A no-op (ignored, not an error) on read-only verbs. |
| `--yes`, `-y` | flag | off | Pre-confirm destructive/writing prompts; required in non-interactive mode for such verbs (§8). |
| `--quiet`, `-q` | flag | auto when non-TTY | Suppress progress/human chrome; data only. Implied when stdout is not a TTY (§6). |
| `--verbose`, `-v` | count | 0 | Raise log verbosity (to **stderr**, never stdout — §6). |
| `--no-color` | flag | auto | Disable ANSI colour; implied when non-TTY or `NO_COLOR` is set. |

Notes carried by neighbours:

- **`--connect` vs `source add 3dam://…` are orthogonal** ([01](01-architecture-and-crates.md) §4): `--connect` chooses *which backend this command talks to*; adding a `3dam://` source adds a federated peer *inside* whatever backend you have (§3, `source`). You can `--connect` to a server and, over that connection, administer *its* federated sources.
- **Auth for `--connect`** is carried by the `ApiClient` and resolved from the OS secret store per host ([10](10-auth-accounts-and-flags.md)); the CLI does not take a password on the command line. `--connect` to a token-gated server reads the stored token; if none exists the command exits `4` (`auth-required`, §7) with a message telling the operator how to store one.

---

## 3. The verb tree (clap)

Structured as clap derive would express it. Each row of §3.9 names the owning `LibraryService` method(s) (§4).

```
3dam
├── scan <SOURCE_ID|PATH>            # ingest / re-index a file source
│     --watch                        #   keep watching after the initial scan
│     --reanalyze                    #   force re-extraction at current extractor version
│     --full                         #   force full (deep) analysis tier, not just cheap metadata
│
├── search <QUERY>                   # text + faceted search (fans out to federated peers)
│     --type <audio|image|model>...  #   media-type facet (repeatable)
│     --tag <TAG>...                 #   tag facet (repeatable, AND)
│     --format <FMT>...              #   concrete-format facet
│     --license <SPDX|Unknown>...    #   license facet (PRODUCT_SPEC §5, §6.3)
│     --usage <commercial|no-attribution|redistributable>...  # rights facet
│     --source <SOURCE_ID>...        #   restrict to sources/peers
│     --limit <N>  --offset <N>      #   pagination (streams within the page)
│     --sort <field[:asc|desc]>
│
├── similar <ASSET_ID | --file <PATH>>   # "more like this" (federated; §6.3)
│     --limit <N>  --min-score <F>
│     --type <…>  --source <…>       #   same facets as search, applied to hits
│
├── convert <ASSET_ID>... | --query <QUERY>   # batch convert/compress/optimise (writes)
│     --to <FORMAT>                  #   target format (owner: [08](08-convert-pipeline.md))
│     --out <DIR>                    #   output location (never overwrites sources; §8)
│     --preset <NAME>                #   codec/quality preset
│     # honours --dry-run (structured diff) and requires --yes (writes; §8)
│
├── tag   <ASSET_ID>... <TAG>...     # add tags (writes; --yes)
├── untag <ASSET_ID>... <TAG>...     # remove tags (writes; --yes)
│
├── set-license <ASSET_ID>... <SPDX|Custom|Unknown>   # writes; --yes
│     --attribution <STRING>         #   credit string
│     --holder <STRING>  --url <URL> #   rights holder / provenance URL
│
├── export                           # metadata / manifest export (read; writes a file)
│     --query <QUERY>                #   subset to export (default: whole library)
│     --format <json|csv|sidecar>    #   manifest shape (owner: [03](03-library-service-and-api.md) DTOs)
│     --out <PATH|-|>                #   file, or "-" for stdout
│     --include <license,tags,features,provenance>...
│
├── source                           # manage sources (file AND federated)
│     ├── add <URI>                  #   local path, sftp://…, smb://…, or 3dam://host:port
│     │     --name <STR>             #     (writes; --yes for network/federated targets)
│     │     --token <ENV_VAR|@keychain>   # federated auth ref (never a literal secret)
│     │     --watch  --no-scan
│     ├── remove <SOURCE_ID>         #   (writes; --yes)  --purge to drop cached rows
│     └── list                       #   id, kind, uri, online/offline, asset count
│
├── serve [CONFIG]                   # long-lived server → dispatched to 3dam-server ([09])
│     --config <PATH>  --bind <ADDR:PORT>  --check   # (surface only; internals in [09])
│
├── mcp                              # stdio MCP server over embedded engine → 3dam-server ([11])
│     --allow-writes                 #   (surface only; tool inventory in [11])
│
└── admin                            # headless server administration (mirrors web admin; PRODUCT_SPEC §6.11)
      ├── flag
      │     ├── list                 #   all feature flags + state + live/restart marker
      │     ├── get <FLAG>
      │     └── set <FLAG> <on|off|VALUE>   # (writes; --yes on consequential flags; §8)
      └── user
            ├── list
            ├── add <NAME> --role <admin|editor|viewer> [--scope <SOURCE|COLLECTION>...]
            ├── set <NAME> [--role <…>] [--scope <…>] [--disable]
            └── remove <NAME>        #   (writes; --yes)
```

### 3.1 Verb → `LibraryService` map

Every non-service verb is a call (or a thin composition) on the `LibraryService` trait ([03](03-library-service-and-api.md)). The CLI adds argument parsing, streaming, formatting, and confirmation — **no library logic**. Method names below are indicative and owned by file 03.

| Verb | `LibraryService` call(s) | Writes? | Federates? |
|------|--------------------------|:------:|:---------:|
| `scan` | `submit_scan(source, opts) -> Stream<ScanEvent>` | local index | no |
| `search` | `query(Query) -> Stream<AssetHit>` | no | **yes** |
| `similar` | `find_similar(ref, opts) -> Stream<AssetHit>` | no | **yes** |
| `convert` | `submit_convert(job) -> Stream<JobEvent>` (job model: [08](08-convert-pipeline.md)) | **yes** (to `--out`) | no |
| `tag` / `untag` | `edit_tags(ids, add, remove)` | **yes** | no |
| `set-license` | `set_license(ids, LicenseInput)` | **yes** | no |
| `export` | `query(…)` + `submit_export(…, format)` | writes a file | yes (subset can span peers) |
| `source add/remove/list` | `add_source` / `remove_source` / `list_sources` | **yes** (add/remove) | n/a |
| `admin flag …` | flag store admin API ([10](10-auth-accounts-and-flags.md)) via serve | **yes** (set) | n/a |
| `admin user …` | accounts admin API ([10](10-auth-accounts-and-flags.md)) via serve | **yes** | n/a |
| `serve` | *not a service call* — starts `3dam-server` ([09](09-server-and-web-client.md)) | — | — |
| `mcp` | *not a service call* — starts stdio MCP ([11](11-mcp-server.md)) over embedded engine | — | — |

`admin` verbs require a backend that exposes the admin API — i.e. an embedded engine that owns a serve store, or a `--connect`ed server where the caller has the `admin` role ([10](10-auth-accounts-and-flags.md)). Against a plain embedded library with no serve store, `admin` exits `3` (`bad-input`) explaining that administration targets a server.

---

## 4. Output modes

One rule decides the mode: **`--json`/`--csv` force machine output; otherwise output is human when stdout is a TTY and machine-quiet when it is not** (DESIGN_GUIDELINES §5: "Detect non-TTY and quiet down automatically").

| Condition | stdout | Progress / chrome |
|-----------|--------|-------------------|
| TTY, no format flag | aligned human **table** (per-media icons, colour) | on stderr, live |
| non-TTY, no format flag | line-oriented plain text (tab-separated, no colour) | suppressed (auto-`--quiet`) |
| `--json` | JSON to stdout (see §4.1) | suppressed on stdout; still on stderr unless `--quiet` |
| `--csv` | CSV (header + rows) to stdout | as above |

Load-bearing details:

- **stdout is data; stderr is chrome.** Progress bars, spinners, logs, and warnings go to **stderr** so `3dam search … --json | jq` is never corrupted. This is why `--verbose` targets stderr (§2).
- **Streaming.** `search`, `similar`, `scan`, and `convert` return streams from the service ([03](03-library-service-and-api.md)). The CLI writes each item as it arrives:
  - `--json` streaming ⇒ **NDJSON** (one JSON object per line) so a consumer can process incrementally; a final summary object carries totals and partial-failure info (§7).
  - `--csv` streaming ⇒ header once, then a row per item.
  - Human streaming ⇒ rows appended under a header; the table auto-sizes to a first bounded batch, then streams.
- **Determinism for CI.** With `--json`/`--csv`, field order and null handling are stable; absent optional values are `null` (JSON) / empty (CSV), never omitted, so schemas are fixed.

### 4.1 JSON envelope

Non-streaming verbs emit a single object; streaming verbs emit NDJSON records followed by one `summary` record. Every JSON document carries the `schema` and `exit`-mirroring `status` so a wrapper can branch without reading the process code.

```jsonc
// non-streaming, e.g. `3dam source list --json`
{ "schema": "3dam.source.list/1", "status": "ok", "sources": [ /* … */ ] }

// streaming, e.g. `3dam search "footstep" --json`  → NDJSON:
{ "schema": "3dam.asset.hit/1", "asset": { /* … */ } }
{ "schema": "3dam.asset.hit/1", "asset": { /* … */ } }
{ "schema": "3dam.search.summary/1", "status": "partial", "returned": 2,
  "peers": { "queried": 3, "answered": 2, "offline": ["3dam://nas:7777"] } }
```

---

## 5. Exit codes

A fixed table so CI can branch (DESIGN_GUIDELINES §5: "exit codes are meaningful"). Codes are stable API; new conditions get new codes rather than remapping old ones. The final `summary` JSON record mirrors the code in `status` for wrappers that parse stdout.

| Code | Name | Meaning | Typical verb |
|:---:|------|---------|--------------|
| `0` | `success` | Completed; all requested work done. | any |
| `1` | `error` | Unexpected/internal failure (I/O, panic-caught, engine error). | any |
| `2` | `not-found` | A named asset / source / flag / user did not exist. | `similar`, `set-license`, `source remove`, `admin` |
| `3` | `bad-input` | Malformed args, bad query, unknown format/flag value, unsupported convert target. | any |
| `4` | `auth-required` | `--connect` target needs auth and none/invalid is stored, or caller lacks the role/scope. | any over `--connect`; `admin` |
| `5` | `source-offline` | The (only) targeted source/peer is unreachable. | `scan`, `search --source`, `source add` |
| `6` | `partial-failure` | Some units succeeded, some failed — e.g. a peer was offline in a fan-out, or some assets in a batch convert failed. Details in the `summary`. | `search`, `similar`, `convert`, `scan`, `tag` |
| `7` | `confirmation-required` | A destructive/writing verb was run non-interactively without `--yes` (§8). | `convert`, `source remove`, `admin flag set`, `set-license` |
| `8` | `nothing-to-do` | Valid request, empty effect (dry-run with no changes; a query matching zero assets when the caller asked for exactly-one). | `convert --dry-run`, `similar` |

Precedence when several apply: `bad-input (3)` (rejected before running) > `auth-required (4)` > `confirmation-required (7)` > run-time outcomes (`2`, `5`, `6`, `8`) > `success (0)`. A fan-out where *every* peer is offline is `5`; where *some* answered is `6`.

---

## 6. No-surprises: `--dry-run` and `--yes`

Applies DESIGN_GUIDELINES §5 ("destructive commands require confirmation or `--yes`; `--dry-run` is available for anything that writes") and the non-destructive invariant (§1.3, PRODUCT_SPEC §6.5).

**Which verbs are "writing".** `convert`, `tag`, `untag`, `set-license`, `export` (writes a file), `source add`/`remove`, and every `admin flag set` / `admin user *` verb. `scan --reanalyze` mutates derived data and is treated as writing to the local store (but not to sources).

**`--dry-run`** — for every writing verb, computes the full effect against the service's dry-run path (e.g. the convert job planner returns a structured diff, [08](08-convert-pipeline.md)) and reports it **without mutating anything**. It prints exactly what a real run would, plus a `"dry_run": true` marker in JSON, and exits `0` (or `8` `nothing-to-do` if the effect is empty). `--dry-run` on a read-only verb is a silent no-op, never an error.

**`--yes` / confirmation** — a writing verb run **interactively** (TTY) prompts `y/N` before mutating; the same verb run **non-interactively** (non-TTY) must carry `--yes` or it refuses with exit `7` `confirmation-required` — no verb ever blocks a script on a prompt (DESIGN_GUIDELINES §1.4: "no interactive prompts required in non-interactive mode"). `--dry-run` never prompts and never needs `--yes` (it writes nothing).

**Consequential admin toggles** — `admin flag set` for flags the admin surface marks *consequential* (exposing the server without auth, enabling MCP or network writes beyond localhost; PRODUCT_SPEC §6.11, DESIGN_GUIDELINES §3.6) require `--yes` even interactively, and the confirmation message repeats the risk note the web admin card would show. Restart-required flags print that they will not take effect until restart (mirroring the web marker).

**Sources are never overwritten.** `convert --out` must resolve to a location outside every source tree; if omitted, output goes to the managed derivative area, never beside the source (PRODUCT_SPEC §6.5, [08](08-convert-pipeline.md)).

---

## 7. Serve and MCP subcommands

`serve` and `mcp` are clap subcommands of the same tree but are **dispatched out of CLI handling into `3dam-server`** by the binary's role classifier — see [01-architecture-and-crates.md](01-architecture-and-crates.md) §5 for the `classify`/`main` mechanism. This file owns only their **command surface**; their internals are elsewhere.

- **`3dam serve [--config PATH] [--bind ADDR:PORT] [--check]`** — starts the long-lived axum service: HTTP/WS API + web host + MCP-over-HTTP on one port, seeded by the config file (which also seeds feature-flag/account state). `--check` validates the config and exits without binding. Everything about routing, live updates, and the config schema is owned by [09-server-and-web-client.md](09-server-and-web-client.md); flag/account/auth semantics by [10-auth-accounts-and-flags.md](10-auth-accounts-and-flags.md). The CLI contributes only argument parsing and the exit code (`0` clean shutdown, `3` bad config, `5` bind address unavailable).
- **`3dam mcp [--allow-writes]`** — runs the MCP tool surface over **stdio** against an **embedded** engine (no network, no running server; PRODUCT_SPEC §6.10). The tool/resource/prompt inventory, write-gating, and the `LibraryService` adapter are owned by [11-mcp-server.md](11-mcp-server.md). Tool names mirror the CLI verbs (§3) so the two surfaces stay learnable together — this parity is a cross-file contract file 11 depends on. `--allow-writes` opts the stdio surface into the gated write tools; default is read-only.

Because both route to `3dam-server` and never re-enter ordinary verb handling, they do **not** accept the run-and-exit output flags (`--json`/`--csv` describe verb results; a service has none). They *do* honour `--config`.

---

## 8. Worked examples

### 8.1 `search` — federated, streaming JSON

```console
$ 3dam --connect nas:7777 search "metallic footstep" \
      --type audio --license CC0-1.0 --usage commercial --limit 2 --json
```
```jsonc
{"schema":"3dam.asset.hit/1","asset":{"id":"a1f3…","name":"footstep_metal_03.wav",
  "type":"audio","format":"wav","source":"local","score":0.94,
  "license":{"id":"CC0-1.0","commercial":true,"attribution":false,"status":"known"},
  "attrs":{"duration_ms":812,"sample_rate":48000,"channels":1}}}
{"schema":"3dam.asset.hit/1","asset":{"id":"3dam://store:7000/b7…","name":"metal_step.ogg",
  "type":"audio","format":"ogg","source":"3dam://store:7000","score":0.88,
  "license":{"id":"CC0-1.0","commercial":true,"attribution":false,"status":"known"}}}
{"schema":"3dam.search.summary/1","status":"partial","returned":2,
  "peers":{"queried":2,"answered":1,"offline":["3dam://teammate:7777"]}}
```
Exit `6` (`partial-failure`) — one peer was offline; the hits that did arrive are usable (fail-soft, PRODUCT_SPEC §8). Interactive/TTY, this renders as a table with an origin-peer column and a warning line on stderr.

### 8.2 `convert` — dry-run then commit

```console
$ 3dam convert --query "type:image format:png" --to ktx2 --out ./out --dry-run --json
{"schema":"3dam.convert.plan/1","dry_run":true,"jobs":3,
 "items":[{"asset":"c1…","from":"png","to":"ktx2","out":"./out/wall_01.ktx2","est_bytes":262144}, …]}
# exit 0

$ 3dam convert --query "type:image format:png" --to ktx2 --out ./out --yes --json
{"schema":"3dam.convert.event/1","asset":"c1…","state":"done","out":"./out/wall_01.ktx2"}
…
{"schema":"3dam.convert.summary/1","status":"ok","converted":3,"failed":0}
# exit 0   (without --yes, non-interactively: exit 7 confirmation-required)
```

### 8.3 `source add` — a federated peer

```console
$ 3dam source add 3dam://store.example:7000 --name asset-store \
      --token STORE_TOKEN --yes --json
{"schema":"3dam.source.add/1","status":"ok",
 "source":{"id":"s4","kind":"federated","uri":"3dam://store.example:7000",
           "online":true,"auth":"token"}}
# exit 0   (unreachable peer → exit 5 source-offline; the source is still recorded offline)
```
`--token STORE_TOKEN` names an **environment variable / keychain ref**, never a literal secret on the command line ([10](10-auth-accounts-and-flags.md)).

### 8.4 `admin` — headless flag + account (mirrors the web admin surface)

```console
$ 3dam --connect nas:7777 admin flag set mcp on --yes --json
{"schema":"3dam.admin.flag/1","status":"ok","flag":"mcp","value":"on","applies":"restart"}

$ 3dam --connect nas:7777 admin user add alice --role editor --scope src:nas-photos --yes
added user 'alice' (editor) scoped to 1 source
```
Setting a *consequential* flag (e.g. `remote-access`, `network-writes`) without `--yes` non-interactively exits `7`; over `--connect` without the `admin` role, exits `4`.

---

## 9. Open questions

> **Resolved 2026-07-06 in [ADR 0009 §6/§7/§10](../adr/0009-v1-scope-decisions.md).** `--json`
> schemas version independently (a `--schema-version` pin is post-v1); the single `partial-failure`
> exit code `6` stands; ship `clap_complete` completions + `clap_mangen` man pages; unify batch-verb
> selection on ids ∪ query ∪ saved-search; embedded `admin` may seed config but not manage live
> sessions; verb↔tool parity via one verb registry. Kept below as rationale.

- **`--json` schema versioning.** Each envelope carries a `schema` id with a version suffix (`/1`). Open: whether these version independently per record type or bump together, and whether a `--schema-version` pin is offered for long-lived CI. Coordinate with the export manifest schema owned by [03](03-library-service-and-api.md).
- **Exit-code granularity for `partial-failure`.** One code (`6`) covers both "a peer was offline in a fan-out" and "some assets in a batch failed". Open: whether CI needs to distinguish these by code, or whether the `summary` record's structured detail suffices (current bet: the record suffices).
- **`admin` reach in embedded mode.** How much administration is meaningful against a *plain* embedded library that has never run `serve` (no serve store): reject entirely, or allow seeding an initial config? Depends on where flag state persists — an open question shared with [10](10-auth-accounts-and-flags.md) and PRODUCT_SPEC §10.
- **Shell completions & man pages.** Whether to ship `clap_complete`-generated completions and `clap_mangen` man pages in the release artifacts ([15](15-observability-config-testing-packaging.md)).
- **`convert`/`export` target-selection ergonomics.** `convert` accepts either explicit ids or `--query`; `export` uses `--query`. Open: whether to unify on a single selection grammar (ids ∪ query ∪ saved-search ref) across all batch verbs.
- **CLI-verb vs MCP-tool-name parity.** CLI verbs (`similar`, `source add`) and the MCP tool names that mirror them (`find_similar`, `add_source` — [11](11-mcp-server.md)) have drifted; §7 states tool names mirror the CLI verbs, but the two surfaces are not yet 1:1. Whether to unify the spellings or keep each surface ergonomically idiomatic is unresolved (shared with [11](11-mcp-server.md)).
