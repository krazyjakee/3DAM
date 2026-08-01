# ADR 0010 — Cargo package naming: `dam-*` packages, `3dam-*` directories, `3dam` binary

Status: **Accepted (draft)** · Date: 2026-07-06 · Deciders: 3DAM core
Supersedes: — · Related: [tech-spec 01](../tech-spec/01-architecture-and-crates.md) §1

## Context

Tech-spec 01 fixes the workspace crate names as `3dam-core`, `3dam-api`, `3dam-render`, … and
the shipped binary as `3dam`. But **Cargo forbids a package name that starts with a digit**
(`error: invalid character '3' in package name … the name cannot start with a digit`), so
`name = "3dam-api"` will not build. The tech-spec's own pseudocode already anticipates this: it
writes imports as `dam_core::EmbeddedLibrary`, `dam_client::ApiClient`, `dam_gui::run()` — i.e.
the **library crates are `dam_*`**, not `3dam_*`.

## Decision

Split the three concerns that "crate name" was conflating:

1. **Cargo package names use the `dam-` prefix** — `dam-api`, `dam-store`, `dam-media`,
   `dam-sources`, `dam-render`, `dam-viewer`, `dam-core`, `dam-client`, `dam-server`,
   `dam-frontend`, `dam-desktop`, `dam-cli`, and `dam` (the binary package). These are cargo-legal, and their
   auto-derived library names are `dam_api`, `dam_core`, … — matching the tech-spec pseudocode
   verbatim.
2. **Directories keep the branded `3dam-` names** — `crates/3dam-api`, `crates/3dam-core`, … —
   so the on-disk layout matches the docs and the memory. Cargo does not require the directory
   name to equal the package name.
3. **The shipped binary is `3dam`** — the `dam` package sets `[[bin]] name = "3dam"`. A binary
   *target* name may start with a digit even though a package name may not, so the user still
   runs `3dam scan …`.

## Consequences

- Code imports read `dam_core`, `dam_api`, `dam_store`, … exactly as tech-spec 01 §4–§5 wrote
  them; nothing in the design pseudocode changes.
- The implemented `cargo xtask check-deps` whitelist references the
  **package** names (`dam-render`, `dam-server`, …), not `3dam-*`.
- Tech-spec 01's crate table should be read as "package `dam-x` in `crates/3dam-x`". The
  `3dam-*` spelling remains the *product/brand* name for the workspace and the binary.
- If we ever want the crates.io/registry names to carry the `3dam` brand, that needs a separate
  decision (publish-time rename or a `3dam`-prefixed alias); not required for v1 (unpublished).
