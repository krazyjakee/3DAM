# API v1 wire contract

`api-v1.json` is a checked-in, language-neutral sample of the JSON exchanged by the Rust API and
the TypeScript web client. It deliberately contains every variant of public wire unions, every
value of the fieldless enums used by the browser, and representative request/response payloads.

The Rust definitions in `dam-api` remain the source of truth. `wire_contract.rs` serializes and
deserializes the fixture and uses exhaustive Rust matches so adding an enum variant forces a
contract update. The web contract test deep-compares the same JSON with a TypeScript fixture;
compile-time assertions make union drift visible during type checking.

When changing a wire type, update the Rust DTO and serde behavior, the corresponding TypeScript
type, this fixture, and both contract tests in the same change. Additive fields should include an
old/minimal decoding assertion where defaults or omission behavior matter.

The fixture is deliberately strict: there is no drift allowlist. If Rust and TypeScript disagree,
the contract/type-check tests fail until both public surfaces converge.

WebSocket subscriptions currently have no resume cursor. `ApiClient` reconnects automatically,
preserves the topic filter, and emits exactly one `stream_lagged` marker for each gap so callers
refresh visible state before applying new events. The reconnect characterization test pins that
behavior until resumable subscriptions are introduced. Each connection mints a one-use ticket over
authenticated HTTP, keeping bearer credentials out of the WebSocket upgrade.
