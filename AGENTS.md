# Repository Guidelines

## Project Structure & Module Organization

3DAM is a Rust 2021 workspace with a React/TypeScript client. Rust packages live under `crates/`; directory names use `3dam-*`, Cargo packages use `dam-*`, and imports use `dam_*`. Keep DTOs and the `LibraryService` contract in `3dam-api`, engine logic in `3dam-core`, persistence in `3dam-store`, and transports in `3dam-server`/`3dam-client`. The Vite client is in `web/src`, separated into components, API bindings, islands, and utilities. Integration tests sit in `crates/*/tests`. Specifications are in `docs/`; experiments belong in `spikes/`, deployment assets in `deploy/`, and automation in `xtask/`.

## Build, Test, and Development Commands

- `cargo build --workspace`: build all native workspace packages.
- `cargo run -p dam -- serve --addr 127.0.0.1:7333`: run the API for web development.
- `cargo test --workspace`: run Rust unit and integration tests.
- `cargo fmt --all --check`: verify Rust formatting.
- `cargo clippy --all-targets -- -D warnings`: lint all Rust targets strictly.
- `cargo xtask ci`: run the canonical pre-push gate (format, lint, tests, and web build).
- `cd web && pnpm install --frozen-lockfile && pnpm dev`: install dependencies and start Vite on port 5173.
- `cd web && pnpm build`: type-check and produce `web/dist/`.

## Coding Style & Naming Conventions

Use standard `rustfmt` output (four-space indentation) and resolve every Clippy warning. Rust modules and functions are `snake_case`; types and traits are `PascalCase`. React components use `PascalCase.tsx`; hooks and utilities use descriptive kebab-case filenames. Reuse Tailwind v4 tokens and the `.btn`/`.field` classes in `web/src/index.css`. Keep Rust DTOs, server/client routes, and `web/src/api/types.ts` synchronized. Preserve dependency boundaries: frontends use `dam-api`, never `dam-store` internals.

## Testing Guidelines

Add focused unit tests near implementation code and behavior-level integration tests under the affected crate's `tests/` directory, using descriptive snake-case filenames such as `waveform_peaks.rs`. Run targeted tests while iterating (`cargo test -p dam-core`) and the full workspace before submission. The web client has no dedicated test framework or coverage threshold; `pnpm lint` and `pnpm typecheck` both enforce TypeScript correctness.

## Commit & Pull Request Guidelines

Recent commits use short, imperative, capability-focused subjects, often with issue references, for example `Add OIDC login, gated by its runtime flag (issue #41)`. Keep each commit cohesive. Pull requests should explain user-visible behavior and architectural impact, link relevant issues or ADRs, list verification commands, and include screenshots or recordings for UI changes. Note schema, API-contract, feature-flag, or deployment changes explicitly.
