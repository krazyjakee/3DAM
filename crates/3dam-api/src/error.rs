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

    /// The wire form. `Internal`'s inner string is never serialised to the client. `RateLimited`
    /// carries its `retry_after` in `detail` so the reconstructed error keeps the backoff hint.
    pub fn to_body(&self) -> ErrorBody {
        let message = match self {
            LibError::Internal(_) => "internal error".to_string(),
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
    pub fn from_body(body: ErrorBody) -> Self {
        match body.code.as_str() {
            "not_found" => LibError::NotFound(body.message),
            "bad_request" => LibError::BadRequest(body.message),
            "unauthorized" => LibError::Unauthorized,
            "forbidden" => LibError::Forbidden(body.message),
            "conflict" => LibError::Conflict(body.message),
            "unsupported" => LibError::Unsupported(body.message),
            "disabled" => LibError::Disabled(body.message),
            "source_unavailable" => LibError::SourceUnavailable(body.message),
            "upstream" => LibError::Upstream(body.message),
            "timeout" => LibError::Timeout,
            "rate_limited" => LibError::RateLimited {
                retry_after: body
                    .detail
                    .as_ref()
                    .and_then(|d| d.get("retry_after"))
                    .and_then(|v| v.as_u64())
                    .map(|v| v as u32)
                    .unwrap_or(0),
            },
            "cancelled" => LibError::Cancelled,
            _ => LibError::Internal(body.message),
        }
    }
}

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
