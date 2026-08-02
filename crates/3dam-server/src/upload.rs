//! The upload transport (issue #80, slice 3) — the one route that writes into a source.
//!
//! **Off by default**, behind its own `Upload` runtime flag. Being the only path that puts bytes in
//! the user's project folders, it is its own exposure class: `Scope::Write` also buys tagging,
//! notes, and collections, so a token minted for those must not silently carry this too. See
//! [`routes`] for why "off" answers 404 rather than 403.
//!
//! **One file per request, body is the bytes.** Not multipart, and that is a deliberate choice
//! rather than an omission:
//!
//! - A multipart parser has to be fed the whole request to find part boundaries, so a batch of
//!   twenty files shares one failure domain: the two-hundredth megabyte of the last file kills the
//!   nineteen that already transferred. One request per file makes fail-soft the *default* — a
//!   rejected file is one 4xx among twenty, and the client already knows which one.
//! - Per-file progress is then just the request's own upload progress, which every browser reports
//!   natively via `XMLHttpRequest.upload`. Modelling the batch server-side as a job would mean
//!   inventing a progress channel to re-describe something the transport already knows.
//! - Cancelling one file is closing one connection.
//!
//! The body is streamed to scratch on real disk (never tmpfs — issue #87) and never held in memory,
//! so a 2 GB model costs a file handle rather than 2 GB of RSS. The staged file is then handed to
//! `LibraryService::upload`, which does the create-only write into the source.
//!
//! The size ceiling is enforced *twice*: once optimistically against `Content-Length`, so an
//! oversized drop is refused before the client streams a single byte of it, and once while
//! streaming, because `Content-Length` is a claim the client makes and a chunked upload omits it
//! entirely.

use crate::auth::Writer;
use crate::{actor_of, ApiError, AppState};
use axum::body::Body;
use axum::extract::{DefaultBodyLimit, Query, State};
use axum::http::HeaderMap;
use axum::routing::post;
use axum::{Json, Router};
use dam_api::dto::{UploadCollision, UploadOutcome, UploadRequest};
use dam_api::id::SourceId;
use dam_api::service::LibraryService;
use dam_api::LibError;
use futures::StreamExt;
use tokio::io::AsyncWriteExt;

/// The upload surface, behind the `Upload` feature flag (off by default — ADR 0004).
///
/// The flag guard is a `route_layer`, so with uploads off the route answers **404, not 403**: off
/// means the surface is *absent*, the same answer `/mcp` and the accounts block give. That is a
/// deliberate reversal of this route's original reasoning, which preferred a 403 "so an
/// unauthorised caller learns why". The two questions turn out to be different ones. *Whether this
/// deployment does uploads at all* is the operator's posture, and a 403 there advertises a
/// capability the operator chose not to run — worth probing for, and a false lead for the client,
/// which would show an Upload view that can only fail. *Whether this caller may upload* is still
/// answered plainly: past the gate, the `Writer` extractor 401s/403s exactly as before, so the
/// caller who is merely under-scoped keeps their explanation.
///
/// `DefaultBodyLimit::disable()` removes axum's 2 MB extractor cap — which would reject essentially
/// every real asset — and the per-request ceiling below replaces it. Disabling it is safe *because*
/// it is replaced: the streaming loop stops reading at the limit.
pub fn routes(state: AppState) -> Router<AppState> {
    Router::new()
        .route(
            "/api/v1/upload",
            post(upload).layer(DefaultBodyLimit::disable()),
        )
        .route_layer(axum::middleware::from_fn_with_state(state, upload_gate))
}

async fn upload_gate(
    State(st): State<AppState>,
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    match st.store.require_upload() {
        Ok(()) => next.run(req).await,
        Err(e) => ApiError(e).into_response(),
    }
}

#[derive(serde::Deserialize)]
pub struct UploadParams {
    source: String,
    #[serde(default)]
    folder: String,
    name: String,
    /// The DTO itself, not a wire-local copy of it: `UploadCollision` already derives `Deserialize`
    /// with snake_case names and `Fail` as its default, and it has no `Overwrite` arm — so
    /// `collision=overwrite` is an unknown variant and a 400 here, exactly as intended, with no
    /// second enum that could drift from the contract (tech-spec 08 §5.1).
    #[serde(default)]
    collision: UploadCollision,
}

async fn upload(
    Writer(ctx): Writer,
    State(st): State<AppState>,
    Query(p): Query<UploadParams>,
    headers: HeaderMap,
    body: Body,
) -> Result<Json<UploadOutcome>, ApiError> {
    let source: SourceId = p.source.parse().map_err(|_| {
        ApiError(LibError::BadRequest(format!(
            "invalid source id {:?}",
            p.source
        )))
    })?;
    let target = format!("{}/{}", p.folder.trim_end_matches('/'), p.name);
    let actor = actor_of(&ctx);

    // Authorise and probe the destination before polling the request body. The engine repeats the
    // visibility/write-grant check after staging — that is the TOCTOU authority if a share is
    // revoked while bytes are in flight — but a caller who is already forbidden must not be able
    // to make the server receive and persist a multi-gigabyte body before learning that fact.
    // `get_source` filters hidden sources before probing their backend, preserving the 404/no-leak
    // contract. Its `writable` answer already intersects backend capability with caller scope and
    // the per-source grant; the explicit grant branch keeps a readable source's refusal a 403.
    let destination = match st.lib.get_source(&ctx, &source).await {
        Ok(destination) => destination,
        Err(error) => {
            audit_refused(&st, &actor, &target, source, 0, &error);
            return Err(ApiError(error));
        }
    };
    if !ctx.visibility.allows_source_write(&source) {
        let error = LibError::Forbidden(
            "no write access to this source (a write share is required)".into(),
        );
        audit_refused(&st, &actor, &target, source, 0, &error);
        return Err(ApiError(error));
    }
    if !destination.writable {
        let error = LibError::Unsupported(format!(
            "source {:?} cannot accept uploads",
            destination.name
        ));
        audit_refused(&st, &actor, &target, source, 0, &error);
        return Err(ApiError(error));
    }

    let ceiling = st.max_upload_bytes;

    // Refuse an oversized upload *before* it transfers. Content-Length is unverified and absent on
    // chunked bodies, so this is an early-out for the honest case, never the enforcement itself.
    if let Some(len) = headers
        .get(axum::http::header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u64>().ok())
    {
        if len > ceiling {
            return Err(ApiError(too_large(len, ceiling)));
        }
    }

    // Stage on real disk under the data dir, never `std::env::temp_dir()` — that is a tmpfs on most
    // Linux hosts and an upload can be gigabytes (issue #87). The temp file is deleted on drop, so
    // a client that disconnects mid-upload leaves nothing behind.
    let scratch = st.lib.scratch_dir();
    tokio::fs::create_dir_all(&scratch)
        .await
        .map_err(|e| ApiError(LibError::Internal(format!("scratch dir: {e}"))))?;
    // The prefix is `dam-sources`' own, so `clean_scratch` sweeps a staged upload orphaned by a
    // crash. A prefix it did not recognise would strand a part-transferred multi-gigabyte file in
    // the data dir permanently.
    let staged = tokio::task::spawn_blocking(move || {
        tempfile::Builder::new()
            .prefix(dam_core::UPLOAD_SCRATCH_PREFIX)
            .tempfile_in(&scratch)
    })
    .await
    .map_err(|e| ApiError(LibError::Internal(e.to_string())))?
    .map_err(|e| ApiError(LibError::Internal(format!("stage upload: {e}"))))?;

    let written = stream_to_file(body, staged.path(), ceiling).await?;

    let req = UploadRequest {
        source,
        folder: p.folder,
        name: p.name,
        collision: p.collision,
    };
    let outcome = match st.lib.upload(&ctx, req, staged.path()).await {
        Ok(o) => o,
        Err(e) => {
            // A *refused* write into a source is at least as interesting as a successful one — a
            // traversal-shaped name or a peer destination is what an audit log is read for after
            // the fact. Recording only successes would leave exactly the attempts worth reviewing
            // invisible.
            audit_refused(&st, &actor, &target, source, written, &e);
            return Err(ApiError(e));
        }
    };

    // Who wrote what, where. Upload is the only way bytes enter a source through 3DAM, so this is
    // exactly the "exposure risk" class of event `audit_log` exists for (tech-spec 10 §5).
    let _ = st.store.audit(
        &actor,
        "source.upload",
        Some(&outcome.path),
        Some(serde_json::json!({
            "source": source.to_string(),
            "bytes": written,
            "skipped": outcome.skipped,
            "asset": outcome.asset.map(|a| a.to_string()),
        })),
    );

    Ok(Json(outcome))
}

fn audit_refused(
    st: &AppState,
    actor: &str,
    target: &str,
    source: SourceId,
    bytes: u64,
    error: &LibError,
) {
    let _ = st.store.audit(
        actor,
        "source.upload.refused",
        Some(target),
        Some(serde_json::json!({
            "source": source.to_string(),
            "bytes": bytes,
            "reason": error.to_string(),
        })),
    );
}

fn too_large(len: u64, ceiling: u64) -> LibError {
    LibError::BadRequest(format!(
        "upload is {len} bytes, over the {ceiling}-byte per-file limit \
         (raise `[upload] max_file_mb` in the serve config)"
    ))
}

/// Stream the request body into `path`, stopping if it exceeds `ceiling`.
///
/// Returns the byte count written. The running total is what actually enforces the limit: a client
/// can lie in `Content-Length` or omit it entirely, so the only trustworthy measure is what we
/// have taken off the socket.
async fn stream_to_file(body: Body, path: &std::path::Path, ceiling: u64) -> Result<u64, ApiError> {
    let mut file = tokio::fs::File::create(path)
        .await
        .map_err(|e| ApiError(LibError::Internal(format!("stage upload: {e}"))))?;
    let mut stream = body.into_data_stream();
    let mut written: u64 = 0;

    while let Some(chunk) = stream.next().await {
        let chunk =
            chunk.map_err(|e| ApiError(LibError::BadRequest(format!("upload body: {e}"))))?;
        written += chunk.len() as u64;
        if written > ceiling {
            return Err(ApiError(too_large(written, ceiling)));
        }
        file.write_all(&chunk)
            .await
            .map_err(|e| ApiError(LibError::Internal(format!("stage upload: {e}"))))?;
    }
    // Flush before the engine opens this path to read it back.
    file.flush()
        .await
        .map_err(|e| ApiError(LibError::Internal(format!("stage upload: {e}"))))?;
    Ok(written)
}
