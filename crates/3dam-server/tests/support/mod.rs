//! Test-only support for the `dam-server` integration tests.
//!
//! Everything under `tests/support/` is compiled **only** into the test binaries — nothing here
//! ships. The house rule these modules follow (set by `crates/3dam-sources/tests/support/mod.rs`,
//! which runs a real SSH+SFTP server on a loopback port rather than mocking `FileSource`) is that a
//! protocol is exercised against a **real implementation of the other side**, over a real socket.
//! A mock proves only that our code calls the functions we told it to call.

pub mod oidc_issuer;
