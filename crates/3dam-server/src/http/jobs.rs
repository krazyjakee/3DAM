//! Background job submission, status, cancellation, and managed artifact delivery.

use super::assets::{parse_range, RangeSpec};
use crate::auth::{Reader, Writer};
use crate::{actor_of, parse_id, ApiError, AppState};
use axum::body::Body;
use axum::extract::{Path as AxPath, State};
use axum::http::{header, HeaderMap, Method, StatusCode};
use axum::response::Response;
use axum::routing::{get, post, MethodRouter};
use axum::Json;
use dam_api::dto::{
    ConvertRequest, ExportFormat, ExportRequest, JobArtifact, JobListRequest, JobResult, JobState,
    JobStatus, ManagedConvertRequest, ManagedExportRequest, ScanRequest,
};
use dam_api::id::JobId;
use dam_api::service::LibraryService;
use dam_api::LibError;
use std::path::PathBuf;
use tokio::io::{AsyncReadExt, AsyncSeekExt};
use tokio_util::io::ReaderStream;

/// Method routers for asynchronous jobs and their server-managed artifacts.
pub(crate) struct Routes {
    pub(crate) scan: MethodRouter<AppState>,
    pub(crate) convert: MethodRouter<AppState>,
    pub(crate) export: MethodRouter<AppState>,
    pub(crate) managed_convert: MethodRouter<AppState>,
    pub(crate) managed_export: MethodRouter<AppState>,
    pub(crate) list: MethodRouter<AppState>,
    pub(crate) get: MethodRouter<AppState>,
    pub(crate) artifact: MethodRouter<AppState>,
    pub(crate) cancel: MethodRouter<AppState>,
}

pub(crate) fn routes() -> Routes {
    Routes {
        scan: post(submit_scan),
        convert: post(submit_convert),
        export: post(submit_export),
        managed_convert: post(submit_managed_convert),
        managed_export: post(submit_managed_export),
        list: post(list_jobs),
        get: get(get_job),
        artifact: get(download_job_artifact).head(download_job_artifact),
        cancel: post(cancel_job),
    }
}

async fn submit_scan(
    Writer(ctx): Writer,
    State(st): State<AppState>,
    Json(req): Json<ScanRequest>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let job_id = st.lib.submit_scan(&ctx, req).await?;
    // The engine/store seam intentionally knows nothing about accounts or token labels. Attribute
    // the durable history row here, where the authenticated actor is available. A failure is
    // returned rather than silently accepting an unattributed job.
    st.lib.set_job_initiator(&job_id, actor_of(&ctx)).await?;
    Ok(Json(serde_json::json!({ "job_id": job_id })))
}

async fn submit_convert(
    Writer(ctx): Writer,
    State(st): State<AppState>,
    Json(req): Json<ConvertRequest>,
) -> Result<(StatusCode, Json<serde_json::Value>), ApiError> {
    let job_id = st.lib.submit_convert(&ctx, req).await?;
    st.lib.set_job_initiator(&job_id, actor_of(&ctx)).await?;
    Ok((
        StatusCode::ACCEPTED,
        Json(serde_json::json!({ "job_id": job_id })),
    ))
}

/// Allocate an opaque, server-owned output directory for a hosted convert. The browser submits no
/// filesystem path; completion is retrieved through `download_job_artifact`, which re-resolves the
/// visible job and validates its structured result against this root.
async fn submit_managed_convert(
    Writer(ctx): Writer,
    State(st): State<AppState>,
    Json(req): Json<ManagedConvertRequest>,
) -> Result<(StatusCode, Json<serde_json::Value>), ApiError> {
    let output = allocate_managed_output(&st.artifacts_dir, "convert", None, true)?;
    let submitted = st
        .lib
        .submit_convert(
            &ctx,
            req.with_output_dir(output.to_string_lossy().into_owned()),
        )
        .await;
    let job_id = match submitted {
        Ok(job) => job,
        Err(error) => {
            let _ = std::fs::remove_dir_all(&output);
            return Err(ApiError(error));
        }
    };
    st.lib.set_job_initiator(&job_id, actor_of(&ctx)).await?;
    Ok((
        StatusCode::ACCEPTED,
        Json(serde_json::json!({ "job_id": job_id })),
    ))
}

async fn submit_export(
    Writer(ctx): Writer,
    State(st): State<AppState>,
    Json(req): Json<ExportRequest>,
) -> Result<(StatusCode, Json<serde_json::Value>), ApiError> {
    let job_id = st.lib.submit_export(&ctx, req).await?;
    st.lib.set_job_initiator(&job_id, actor_of(&ctx)).await?;
    Ok((
        StatusCode::ACCEPTED,
        Json(serde_json::json!({ "job_id": job_id })),
    ))
}

/// Allocate a server-owned manifest destination. JSON/CSV land as one exact file; sidecars land in
/// a fresh directory and are packaged only when the authenticated user asks to download the
/// completed job. The public request intentionally cannot supply or influence the path.
async fn submit_managed_export(
    Writer(ctx): Writer,
    State(st): State<AppState>,
    Json(req): Json<ManagedExportRequest>,
) -> Result<(StatusCode, Json<serde_json::Value>), ApiError> {
    let extension = match req.format {
        ExportFormat::Json => Some("json"),
        ExportFormat::Csv => Some("csv"),
        ExportFormat::Sidecar => None,
    };
    let output = allocate_managed_output(
        &st.artifacts_dir,
        "export",
        extension,
        matches!(req.format, ExportFormat::Sidecar),
    )?;
    let submitted = st
        .lib
        .submit_export(&ctx, req.with_output(output.to_string_lossy().into_owned()))
        .await;
    let job_id = match submitted {
        Ok(job) => job,
        Err(error) => {
            if output.is_dir() {
                let _ = std::fs::remove_dir_all(&output);
            } else {
                let _ = std::fs::remove_file(&output);
            }
            return Err(ApiError(error));
        }
    };
    st.lib.set_job_initiator(&job_id, actor_of(&ctx)).await?;
    Ok((
        StatusCode::ACCEPTED,
        Json(serde_json::json!({ "job_id": job_id })),
    ))
}

fn allocate_managed_output(
    root: &std::path::Path,
    kind: &str,
    extension: Option<&str>,
    directory: bool,
) -> Result<PathBuf, ApiError> {
    let parent = root.join(kind);
    std::fs::create_dir_all(&parent).map_err(|error| {
        ApiError(LibError::Internal(format!(
            "create managed artifact directory: {error}"
        )))
    })?;
    // A fresh UUIDv7 is an allocation token, not the eventual job id. It makes the destination
    // unguessable enough to avoid collisions while the authenticated route still keys by job id.
    let token = JobId::new().to_string();
    let output = match extension {
        Some(extension) => parent.join(format!("{token}.{extension}")),
        None => parent.join(token),
    };
    if directory {
        std::fs::create_dir(&output).map_err(|error| {
            ApiError(LibError::Internal(format!(
                "create managed artifact output: {error}"
            )))
        })?;
    }
    Ok(output)
}

async fn get_job(
    Reader(ctx): Reader,
    State(st): State<AppState>,
    AxPath(id): AxPath<String>,
) -> Result<Json<JobStatus>, ApiError> {
    let id: JobId = parse_id(&id, "job")?;
    let mut job = st.lib.get_job(&ctx, &id).await?;
    decorate_managed_job(&st.artifacts_dir, &mut job);
    Ok(Json(job))
}

async fn list_jobs(
    Reader(ctx): Reader,
    State(st): State<AppState>,
    Json(req): Json<JobListRequest>,
) -> Result<Json<dam_api::page::Page<JobStatus>>, ApiError> {
    let mut page = st.lib.list_jobs(&ctx, req).await?;
    for job in &mut page.items {
        decorate_managed_job(&st.artifacts_dir, job);
    }
    Ok(Json(page))
}

fn decorate_managed_job(root: &std::path::Path, job: &mut JobStatus) {
    if job.state != JobState::Done {
        return;
    }
    let Some(result) = job.result.as_deref_mut() else {
        return;
    };
    let (reported, downloadable, label) = match result {
        JobResult::Export(report) => (&report.output, true, "Download manifest"),
        JobResult::Convert(report) => (
            &report.output_dir,
            !report.dry_run && report.done > 0,
            "Download converted files",
        ),
    };
    if !managed_path_belongs_to(root, std::path::Path::new(reported)) {
        return;
    }
    let route = format!("/api/v1/jobs/{}/artifact", job.id);
    if downloadable
        && !job
            .result_artifacts
            .iter()
            .any(|artifact| artifact.route.as_deref() == Some(route.as_str()))
    {
        job.result_artifacts.push(JobArtifact {
            label: label.into(),
            route: Some(route),
        });
    }

    // A managed path is an implementation detail, not useful locality information. Keep only file
    // names in item rows and use an explicit delivery label for the destination.
    match result {
        JobResult::Export(report) => report.output = "Server-managed download".into(),
        JobResult::Convert(report) => {
            report.output_dir = if report.dry_run {
                "No output (dry run)".into()
            } else {
                "Server-managed download".into()
            };
            for item in &mut report.items {
                if let Some(name) = std::path::Path::new(&item.planned_output).file_name() {
                    item.planned_output = name.to_string_lossy().into_owned();
                }
            }
        }
    }
}

/// Containment for result decoration also works before a file exists (for example an empty convert
/// or a dry run): canonicalize the managed root and the nearest existing parent, then append only a
/// single final file name. A parent component, absolute replacement, or symlink escape fails shut.
fn managed_path_belongs_to(root: &std::path::Path, candidate: &std::path::Path) -> bool {
    let Ok(relative) = candidate.strip_prefix(root) else {
        return false;
    };
    if relative.as_os_str().is_empty()
        || relative
            .components()
            .any(|component| !matches!(component, std::path::Component::Normal(_)))
    {
        return false;
    }
    let Ok(root) = std::fs::canonicalize(root) else {
        return false;
    };
    if let Ok(candidate) = std::fs::canonicalize(candidate) {
        return candidate != root && candidate.starts_with(&root);
    }
    let Some(parent) = candidate.parent() else {
        return false;
    };
    let Ok(parent) = std::fs::canonicalize(parent) else {
        return false;
    };
    candidate.file_name().is_some() && parent.starts_with(root)
}

async fn download_job_artifact(
    Reader(ctx): Reader,
    State(st): State<AppState>,
    AxPath(id): AxPath<String>,
    method: Method,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    let id: JobId = parse_id(&id, "job")?;
    // This is the authorization and visibility choke point. The route never accepts a path and
    // never trusts a client copy of the report: it re-reads the durable job for this caller.
    let job = st.lib.get_job(&ctx, &id).await?;
    if job.state != JobState::Done {
        return Err(ApiError(LibError::Conflict(
            "the job has no completed artifact".into(),
        )));
    }
    let result = job
        .result
        .as_deref()
        .ok_or_else(|| ApiError(LibError::NotFound("job artifact".into())))?;
    let (reported, filename, content_type, package) = match result {
        JobResult::Export(report) => match report.format {
            ExportFormat::Json => (
                std::path::Path::new(&report.output),
                format!("3dam-manifest-{id}.json"),
                "application/json",
                false,
            ),
            ExportFormat::Csv => (
                std::path::Path::new(&report.output),
                format!("3dam-manifest-{id}.csv"),
                "text/csv; charset=utf-8",
                false,
            ),
            ExportFormat::Sidecar => (
                std::path::Path::new(&report.output),
                format!("3dam-sidecars-{id}.zip"),
                "application/zip",
                true,
            ),
        },
        JobResult::Convert(report) if !report.dry_run && report.done > 0 => (
            std::path::Path::new(&report.output_dir),
            format!("3dam-convert-{id}.zip"),
            "application/zip",
            true,
        ),
        JobResult::Convert(_) => return Err(ApiError(LibError::NotFound("job artifact".into()))),
    };
    let source = canonical_managed_artifact(&st.artifacts_dir, reported)?;
    let file = if package {
        package_managed_directory(&st.artifacts_dir, &source, &id).await?
    } else {
        if !std::fs::metadata(&source)
            .map(|m| m.is_file())
            .unwrap_or(false)
        {
            return Err(ApiError(LibError::NotFound("job artifact".into())));
        }
        source
    };
    stream_artifact(file, filename, content_type, id, method, headers).await
}

fn canonical_managed_artifact(
    root: &std::path::Path,
    candidate: &std::path::Path,
) -> Result<PathBuf, ApiError> {
    let root = std::fs::canonicalize(root)
        .map_err(|_| ApiError(LibError::NotFound("job artifact".into())))?;
    let candidate = std::fs::canonicalize(candidate)
        .map_err(|_| ApiError(LibError::NotFound("job artifact".into())))?;
    if candidate == root || !candidate.starts_with(&root) {
        return Err(ApiError(LibError::NotFound("job artifact".into())));
    }
    Ok(candidate)
}

async fn package_managed_directory(
    root: &std::path::Path,
    directory: &std::path::Path,
    job: &JobId,
) -> Result<PathBuf, ApiError> {
    if !std::fs::metadata(directory)
        .map(|m| m.is_dir())
        .unwrap_or(false)
    {
        return Err(ApiError(LibError::NotFound("job artifact".into())));
    }
    let packages = root.join("packages");
    std::fs::create_dir_all(&packages).map_err(|error| {
        ApiError(LibError::Internal(format!(
            "create artifact package directory: {error}"
        )))
    })?;
    let destination = packages.join(format!("{job}.zip"));
    if destination.is_file() {
        return canonical_managed_artifact(root, &destination);
    }
    let directory = directory.to_path_buf();
    let destination_for_task = destination.clone();
    tokio::task::spawn_blocking(move || -> Result<(), LibError> {
        let temp = tempfile::Builder::new()
            .prefix(".3dam-package-")
            .tempfile_in(
                destination_for_task
                    .parent()
                    .expect("managed package has a parent"),
            )
            .map_err(|error| LibError::Internal(format!("stage artifact package: {error}")))?;
        let mut archive = zip::ZipWriter::new(temp);
        let options = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Deflated);
        let entries = std::fs::read_dir(&directory)
            .map_err(|error| LibError::Internal(format!("read artifact directory: {error}")))?;
        for entry in entries {
            let entry = entry
                .map_err(|error| LibError::Internal(format!("read artifact entry: {error}")))?;
            let file_type = entry
                .file_type()
                .map_err(|error| LibError::Internal(format!("inspect artifact entry: {error}")))?;
            // Managed multi-file outputs are flat today. Refuse symlinks, directories, devices,
            // and anything else rather than recursively acquiring unrelated server content.
            if !file_type.is_file() {
                return Err(LibError::NotFound("job artifact".into()));
            }
            let name = safe_archive_name(&entry.file_name())?;
            archive.start_file(name, options).map_err(|error| {
                LibError::Internal(format!("start artifact zip entry: {error}"))
            })?;
            let mut input = std::fs::File::open(entry.path())
                .map_err(|error| LibError::Internal(format!("open artifact entry: {error}")))?;
            std::io::copy(&mut input, &mut archive).map_err(|error| {
                LibError::Internal(format!("write artifact zip entry: {error}"))
            })?;
        }
        let temp = archive
            .finish()
            .map_err(|error| LibError::Internal(format!("finish artifact package: {error}")))?;
        if let Err(error) = temp.persist(&destination_for_task) {
            // Two authenticated downloads may build the same immutable package concurrently. The
            // winner is already the complete representation; losing an atomic publish race is
            // success once the destination is a regular file (not a 500 on double-click).
            if !destination_for_task.is_file() {
                return Err(LibError::Internal(format!(
                    "publish artifact package: {}",
                    error.error
                )));
            }
        }
        Ok(())
    })
    .await
    .map_err(|error| {
        ApiError(LibError::Internal(format!(
            "package artifact task: {error}"
        )))
    })??;
    canonical_managed_artifact(root, &destination)
}

fn safe_archive_name(name: &std::ffi::OsStr) -> Result<String, LibError> {
    let Some(name) = name.to_str() else {
        return Err(LibError::NotFound("job artifact".into()));
    };
    // ZIP consumers disagree on whether `\` is data or a separator. Restrict names to a portable
    // single component so an archive created on Unix cannot traverse when extracted on Windows.
    let portable = !name.is_empty()
        && name != "."
        && name != ".."
        && !name.ends_with('.')
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'_'));
    portable
        .then_some(name.to_owned())
        .ok_or_else(|| LibError::NotFound("job artifact".into()))
}

async fn stream_artifact(
    path: PathBuf,
    filename: String,
    content_type: &'static str,
    job: JobId,
    method: Method,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    let mut file = tokio::fs::File::open(&path)
        .await
        .map_err(|_| ApiError(LibError::NotFound("job artifact".into())))?;
    let len = file
        .metadata()
        .await
        .map_err(|_| ApiError(LibError::NotFound("job artifact".into())))?
        .len();
    let etag = format!("\"artifact-{job}\"");
    let range = headers
        .get(header::RANGE)
        .and_then(|value| value.to_str().ok())
        .filter(|_| {
            headers
                .get(header::IF_RANGE)
                .and_then(|value| value.to_str().ok())
                .is_none_or(|value| value.trim() == etag)
        });
    let (status, first, last) = if method == Method::HEAD {
        (StatusCode::OK, 0, len.saturating_sub(1))
    } else {
        match range.map(|value| parse_range(value, len)) {
            Some(RangeSpec::Satisfiable(first, last)) => (StatusCode::PARTIAL_CONTENT, first, last),
            Some(RangeSpec::Unsatisfiable) => {
                let mut response = Response::new(Body::empty());
                *response.status_mut() = StatusCode::RANGE_NOT_SATISFIABLE;
                response.headers_mut().insert(
                    header::CONTENT_RANGE,
                    header::HeaderValue::from_str(&format!("bytes */{len}")).unwrap(),
                );
                return Ok(response);
            }
            Some(RangeSpec::Ignore) | None => (StatusCode::OK, 0, len.saturating_sub(1)),
        }
    };
    let body_len = if len == 0 { 0 } else { last - first + 1 };
    if method != Method::HEAD && body_len > 0 {
        file.seek(std::io::SeekFrom::Start(first))
            .await
            .map_err(|error| ApiError(LibError::Internal(format!("seek artifact: {error}"))))?;
    }
    let body = if method == Method::HEAD || body_len == 0 {
        Body::empty()
    } else {
        Body::from_stream(ReaderStream::new(file.take(body_len)))
    };
    let mut response = Response::new(body);
    *response.status_mut() = status;
    let h = response.headers_mut();
    h.insert(
        header::CONTENT_TYPE,
        header::HeaderValue::from_static(content_type),
    );
    h.insert(
        header::CONTENT_DISPOSITION,
        header::HeaderValue::from_str(&format!("attachment; filename=\"{filename}\""))
            .expect("job-derived filename is a safe header value"),
    );
    h.insert(
        header::ACCEPT_RANGES,
        header::HeaderValue::from_static("bytes"),
    );
    h.insert(
        header::CACHE_CONTROL,
        header::HeaderValue::from_static("private, no-store"),
    );
    h.insert(header::ETAG, header::HeaderValue::from_str(&etag).unwrap());
    h.insert(
        header::CONTENT_LENGTH,
        header::HeaderValue::from_str(&body_len.to_string()).unwrap(),
    );
    h.insert(
        header::HeaderName::from_static("x-content-type-options"),
        header::HeaderValue::from_static("nosniff"),
    );
    if status == StatusCode::PARTIAL_CONTENT {
        h.insert(
            header::CONTENT_RANGE,
            header::HeaderValue::from_str(&format!("bytes {first}-{last}/{len}")).unwrap(),
        );
    }
    Ok(response)
}

async fn cancel_job(
    Writer(ctx): Writer,
    State(st): State<AppState>,
    AxPath(id): AxPath<String>,
) -> Result<StatusCode, ApiError> {
    let id: JobId = parse_id(&id, "job")?;
    st.lib.cancel_job(&ctx, &id).await?;
    Ok(StatusCode::NO_CONTENT)
}
