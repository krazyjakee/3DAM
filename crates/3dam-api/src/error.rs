//! The typed error model (tech-spec 03 §5).
//!
//! `LibError` is reserved for **hard failures of the whole call**. Soft, per-item / per-peer
//! degradation rides in [`crate::page::PartialStatus`] on a *successful* result — that split is
//! the fail-soft rule made concrete. `ErrorBody` is the wire form; `code` is machine-stable so a
//! connected client can reconstruct the same variant an embedded client would have seen.

use serde::{Deserialize, Serialize};

#[derive(thiserror::Error, Debug, Clone, PartialEq, Eq)]
pub enum LibError {
    #[error("not found: {0}")]
    NotFound(String),
    #[error("invalid request: {0}")]
    BadRequest(String),
    #[error("unauthorized")]
    Unauthorized,
    #[error("forbidden: {0}")]
    Forbidden(String),
    #[error("conflict: {0}")]
    Conflict(String),
    #[error("unsupported: {0}")]
    Unsupported(String),
    #[error("capability disabled: {0}")]
    Disabled(String),
    #[error("source unavailable: {0}")]
    SourceUnavailable(String),
    #[error("upstream failed: {0}")]
    Upstream(String),
    #[error("timed out")]
    Timeout,
    #[error("rate limited")]
    RateLimited { retry_after: u32 },
    #[error("cancelled")]
    Cancelled,
    #[error("internal: {0}")]
    Internal(String),
}

impl LibError {
    /// Machine-stable code string, carried over the wire in [`ErrorBody`].
    pub fn code(&self) -> &'static str {
        match self {
            LibError::NotFound(_) => "not_found",
            LibError::BadRequest(_) => "bad_request",
            LibError::Unauthorized => "unauthorized",
            LibError::Forbidden(_) => "forbidden",
            LibError::Conflict(_) => "conflict",
            LibError::Unsupported(_) => "unsupported",
            LibError::Disabled(_) => "disabled",
            LibError::SourceUnavailable(_) => "source_unavailable",
            LibError::Upstream(_) => "upstream",
            LibError::Timeout => "timeout",
            LibError::RateLimited { .. } => "rate_limited",
            LibError::Cancelled => "cancelled",
            LibError::Internal(_) => "internal",
        }
    }

    /// HTTP status this variant maps to (tech-spec 03 §5.1 / §8.4).
    pub fn http_status(&self) -> u16 {
        match self {
            LibError::NotFound(_) => 404,
            LibError::BadRequest(_) => 400,
            LibError::Unauthorized => 401,
            LibError::Forbidden(_) | LibError::Disabled(_) => 403,
            LibError::Conflict(_) => 409,
            LibError::Unsupported(_) => 422,
            LibError::SourceUnavailable(_) | LibError::Upstream(_) => 502,
            LibError::Timeout => 504,
            LibError::RateLimited { .. } => 429,
            LibError::Cancelled => 499,
            LibError::Internal(_) => 500,
        }
    }

    /// The category prefix this variant's `Display` prepends to its payload — the inverse of the
    /// `#[error(...)]` attributes above, keyed by [`Self::code`]. `None` for the payload-free
    /// variants (and for codes we don't know), whose message is the whole rendering.
    fn category_prefix(code: &str) -> Option<&'static str> {
        Some(match code {
            "not_found" => "not found: ",
            "bad_request" => "invalid request: ",
            "forbidden" => "forbidden: ",
            "conflict" => "conflict: ",
            "unsupported" => "unsupported: ",
            "disabled" => "capability disabled: ",
            "source_unavailable" => "source unavailable: ",
            "upstream" => "upstream failed: ",
            "internal" => "internal: ",
            _ => return None,
        })
    }

    /// The wire form. `message` is the *fully rendered* error, because clients that never
    /// reconstruct a `LibError` show it verbatim (the web client puts it straight in the UI) — so
    /// it has to be self-describing. `from_body` undoes the prefix rather than the payload being
    /// shipped bare.
    ///
    /// `Internal`'s inner string is never serialised to the client: the *payload* is redacted and
    /// then rendered, so the wire message keeps the same `"<category>: <detail>"` shape as every
    /// other variant and survives the round-trip. `RateLimited` carries its `retry_after` in
    /// `detail` so the reconstructed error keeps the backoff hint.
    pub fn to_body(&self) -> ErrorBody {
        let message = match self {
            LibError::Internal(_) => LibError::Internal(INTERNAL_REDACTED.to_string()).to_string(),
            other => other.to_string(),
        };
        let detail = match self {
            LibError::RateLimited { retry_after } => {
                Some(serde_json::json!({ "retry_after": retry_after }))
            }
            _ => None,
        };
        ErrorBody {
            code: self.code().to_string(),
            message,
            detail,
        }
    }

    /// Reconstruct from a wire body (API-client side). Unknown codes become `Internal`.
    ///
    /// `body.message` is already rendered *with* its category prefix, and each variant's `Display`
    /// prepends that prefix again — so the prefix is stripped here first. Without it a connected
    /// client printed `"not found: not found: account nope"` where the embedded one printed
    /// `"not found: account nope"`.
    pub fn from_body(body: ErrorBody) -> Self {
        let ErrorBody {
            code,
            message,
            detail,
        } = body;
        // Only strip the prefix this exact code renders; an unknown code keeps its message whole.
        let payload = Self::category_prefix(&code)
            .and_then(|p| message.strip_prefix(p).map(str::to_string))
            .unwrap_or(message);
        match code.as_str() {
            "not_found" => LibError::NotFound(payload),
            "bad_request" => LibError::BadRequest(payload),
            "unauthorized" => LibError::Unauthorized,
            "forbidden" => LibError::Forbidden(payload),
            "conflict" => LibError::Conflict(payload),
            "unsupported" => LibError::Unsupported(payload),
            "disabled" => LibError::Disabled(payload),
            "source_unavailable" => LibError::SourceUnavailable(payload),
            "upstream" => LibError::Upstream(payload),
            "timeout" => LibError::Timeout,
            "rate_limited" => LibError::RateLimited {
                retry_after: detail
                    .as_ref()
                    .and_then(|d| d.get("retry_after"))
                    .and_then(|v| v.as_u64())
                    .map(|v| v as u32)
                    .unwrap_or(0),
            },
            "cancelled" => LibError::Cancelled,
            _ => LibError::Internal(payload),
        }
    }
}

/// What `Internal`'s payload becomes on the wire — the real one can quote a path or a SQL
/// statement, so it never leaves the server.
const INTERNAL_REDACTED: &str = "unexpected failure";

/// Map any `Display` error into an opaque `LibError::Internal` — the workspace-wide shorthand for
/// `.map_err(internal)` when a lower-level failure has no better-typed variant.
pub fn internal<E: std::fmt::Display>(e: E) -> LibError {
    LibError::Internal(e.to_string())
}

/// Wire error envelope (tech-spec 03 §5.2).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ErrorBody {
    pub code: String,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub detail: Option<serde_json::Value>,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One instance of every variant. The `match` is the exhaustiveness guard: adding a variant to
    /// `LibError` stops compiling here until it is listed above too.
    fn every_variant() -> Vec<LibError> {
        let all = vec![
            LibError::NotFound("account nope".into()),
            LibError::BadRequest("password must be at least 8 characters".into()),
            LibError::Unauthorized,
            LibError::Forbidden("scope admin required".into()),
            LibError::Conflict("that provider subject is already linked to an account".into()),
            LibError::Unsupported("cannot convert video".into()),
            LibError::Disabled("remote access".into()),
            LibError::SourceUnavailable("sftp host offline".into()),
            LibError::Upstream("peer returned 500".into()),
            LibError::Timeout,
            LibError::RateLimited { retry_after: 42 },
            LibError::Cancelled,
            LibError::Internal("db locked at /home/j/library.db".into()),
        ];
        for e in &all {
            match e {
                LibError::NotFound(_)
                | LibError::BadRequest(_)
                | LibError::Unauthorized
                | LibError::Forbidden(_)
                | LibError::Conflict(_)
                | LibError::Unsupported(_)
                | LibError::Disabled(_)
                | LibError::SourceUnavailable(_)
                | LibError::Upstream(_)
                | LibError::Timeout
                | LibError::RateLimited { .. }
                | LibError::Cancelled
                | LibError::Internal(_) => {}
            }
        }
        all
    }

    /// The connected (`--connect`) path must render exactly what the embedded (`--data`) path
    /// does: `to_body` ships the rendered message, so `from_body` has to strip the category prefix
    /// before `Display` prepends it again.
    #[test]
    fn the_wire_round_trip_renders_the_category_prefix_exactly_once() {
        for e in every_variant() {
            let body = e.to_body();
            let back = LibError::from_body(body.clone());
            assert_eq!(back.code(), e.code());
            assert_eq!(back.http_status(), e.http_status());
            // A client that only reads `ErrorBody.message` (the web client) and one that
            // reconstructs the error (the CLI over `--connect`) show the same string.
            assert_eq!(back.to_string(), body.message, "code {}", body.code);
            match e {
                // `Internal` is the one variant that cannot round-trip its payload: it is
                // redacted on purpose. Its *category* still renders once.
                LibError::Internal(_) => {
                    assert_eq!(back.to_string(), format!("internal: {INTERNAL_REDACTED}"));
                }
                other => assert_eq!(back.to_string(), other.to_string()),
            }
        }
    }

    #[test]
    fn the_internal_payload_never_reaches_the_wire() {
        let body = LibError::Internal("db locked at /home/j/library.db".into()).to_body();
        assert!(!body.message.contains("library.db"));
    }

    #[test]
    fn rate_limited_keeps_its_backoff_hint() {
        let back = LibError::from_body(LibError::RateLimited { retry_after: 42 }.to_body());
        assert_eq!(back, LibError::RateLimited { retry_after: 42 });
    }

    /// A code from a newer peer keeps its whole message — there is no prefix we know to remove.
    #[test]
    fn an_unknown_code_falls_back_to_internal_with_the_message_intact() {
        let back = LibError::from_body(ErrorBody {
            code: "teapot".into(),
            message: "short and stout".into(),
            detail: None,
        });
        assert_eq!(back, LibError::Internal("short and stout".into()));
    }
}
