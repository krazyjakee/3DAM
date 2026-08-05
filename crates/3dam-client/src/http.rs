//! HTTP transport plumbing for [`ApiClient`] — connection setup, URL derivation, response
//! decoding, and the typed `get`/`post`/`put`/`delete` verb helpers every other client method is
//! written in terms of. Nothing here knows what a catalog is: it turns a path plus a body into a
//! deserialized reply or a `LibError`. The `LibraryService` implementation and the admin surface
//! sit on top of it.

use crate::{media_from_content_type, ApiClient, JobIdReply};
use dam_api::dto::{AssetContentMetadata, ManagedConvertRequest, ManagedExportRequest};
use dam_api::id::{JobId, SourceId};
use dam_api::{ErrorBody, LibError, PeerAdvertise};
use futures::StreamExt;
use serde::de::DeserializeOwned;
use serde::Serialize;
use url::Url;

impl ApiClient {
    /// Connect to a remote server without a credential (auth `Off`/`Anonymous` peers).
    pub async fn connect(endpoint: Url) -> Result<ApiClient, LibError> {
        Self::connect_with_token(endpoint, None).await
    }

    /// Connect presenting a bearer `token` on authenticated HTTP requests (required for a
    /// Token-mode peer). WebSocket subscriptions use that client to mint a one-use ticket; the
    /// bearer itself is never copied into the upgrade request.
    pub async fn connect_with_token(
        endpoint: Url,
        token: Option<String>,
    ) -> Result<ApiClient, LibError> {
        let mut builder = reqwest::Client::builder();
        if let Some(t) = &token {
            let mut headers = reqwest::header::HeaderMap::new();
            let value = reqwest::header::HeaderValue::from_str(&format!("Bearer {t}"))
                .map_err(|e| LibError::BadRequest(format!("invalid token: {e}")))?;
            headers.insert(reqwest::header::AUTHORIZATION, value);
            builder = builder.default_headers(headers);
        }
        let http = builder
            .build()
            .map_err(|e| LibError::Internal(e.to_string()))?;
        Ok(ApiClient {
            base: endpoint,
            http,
        })
    }

    /// Submit a hosted conversion whose destination is allocated by the server. This is
    /// transport-specific (an embedded `LibraryService` has no remote artifact boundary), so it is
    /// an inherent client method rather than part of the shared engine trait.
    pub async fn submit_managed_convert(
        &self,
        req: ManagedConvertRequest,
    ) -> Result<JobId, LibError> {
        let reply: JobIdReply = self.post("/api/v1/jobs/convert-artifact", &req).await?;
        Ok(reply.job_id)
    }

    /// Submit a hosted manifest whose destination is allocated by the server.
    pub async fn submit_managed_export(
        &self,
        req: ManagedExportRequest,
    ) -> Result<JobId, LibError> {
        let reply: JobIdReply = self.post("/api/v1/jobs/export-artifact", &req).await?;
        Ok(reply.job_id)
    }

    /// Retrieve the exact artifact authorized by a completed visible job. The returned MIME type
    /// distinguishes JSON/CSV from packaged multi-file outputs; the server owns the filename.
    pub async fn download_job_artifact(
        &self,
        job: &JobId,
    ) -> Result<(Option<String>, Vec<u8>), LibError> {
        self.fetch_bytes(
            self.http
                .get(self.url(&format!("/api/v1/jobs/{job}/artifact"))?),
        )
        .await
    }

    pub(crate) fn url(&self, path: &str) -> Result<Url, LibError> {
        self.base
            .join(path)
            .map_err(|e| LibError::BadRequest(e.to_string()))
    }

    pub(crate) fn routed_url(&self, path: &str, source: Option<SourceId>) -> Result<Url, LibError> {
        let mut url = self.url(path)?;
        if let Some(source) = source {
            url.query_pairs_mut()
                .append_pair("source", &source.to_string());
        }
        Ok(url)
    }

    /// The `ws://` / `wss://` URL for the live-event endpoint, derived from the http(s) base.
    pub(crate) fn ws_url(&self) -> Result<Url, LibError> {
        let mut u = self.url("/api/v1/ws")?;
        let ws_scheme = if u.scheme() == "https" { "wss" } else { "ws" };
        u.set_scheme(ws_scheme)
            .map_err(|_| LibError::Internal("cannot derive ws scheme".into()))?;
        Ok(u)
    }

    pub(crate) async fn decode<T: DeserializeOwned>(
        resp: reqwest::Response,
    ) -> Result<T, LibError> {
        let status = resp.status();
        let bytes = resp
            .bytes()
            .await
            .map_err(|e| LibError::Upstream(e.to_string()))?;
        if status.is_success() {
            serde_json::from_slice(&bytes).map_err(|e| LibError::Internal(e.to_string()))
        } else {
            match serde_json::from_slice::<ErrorBody>(&bytes) {
                Ok(body) => Err(LibError::from_body(body)),
                Err(_) => Err(LibError::Upstream(format!("HTTP {}", status.as_u16()))),
            }
        }
    }

    pub(crate) async fn expect_no_content(resp: reqwest::Response) -> Result<(), LibError> {
        let status = resp.status();
        if status.is_success() {
            return Ok(());
        }
        let bytes = resp.bytes().await.unwrap_or_default();
        match serde_json::from_slice::<ErrorBody>(&bytes) {
            Ok(body) => Err(LibError::from_body(body)),
            Err(_) => Err(LibError::Upstream(format!("HTTP {}", status.as_u16()))),
        }
    }

    /// Send a GET expecting raw bytes (not JSON): map a transport failure to `SourceUnavailable`, an
    /// error body / non-2xx to the right `LibError`, and on success return the response
    /// `Content-Type` (if present) with the body. Callers reconstruct the typed `AssetContent`.
    pub(crate) async fn fetch_bytes(
        &self,
        req: reqwest::RequestBuilder,
    ) -> Result<(Option<String>, Vec<u8>), LibError> {
        let resp = req
            .send()
            .await
            .map_err(|e| LibError::SourceUnavailable(e.to_string()))?;
        let status = resp.status();
        if !status.is_success() {
            let bytes = resp.bytes().await.unwrap_or_default();
            return match serde_json::from_slice::<ErrorBody>(&bytes) {
                Ok(body) => Err(LibError::from_body(body)),
                Err(_) => Err(LibError::Upstream(format!("HTTP {}", status.as_u16()))),
            };
        }
        let content_type = resp
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .map(|s| s.to_string());
        let bytes = resp
            .bytes()
            .await
            .map_err(|e| LibError::Upstream(e.to_string()))?
            .to_vec();
        Ok((content_type, bytes))
    }

    /// Materialise a raw response while enforcing the cap against both the declared and actual
    /// bytes. The prior `HEAD` is only an optimisation: a changed or dishonest peer cannot turn it
    /// into an unbounded `Response::bytes()` allocation.
    pub(crate) async fn fetch_bytes_bounded(
        &self,
        req: reqwest::RequestBuilder,
        ceiling: u64,
    ) -> Result<(Option<String>, Vec<u8>), LibError> {
        let response = req
            .send()
            .await
            .map_err(|error| LibError::SourceUnavailable(error.to_string()))?;
        let status = response.status();
        if !status.is_success() {
            // Do not materialise an untrusted error body on the content path. The status is enough
            // to fail this compatibility read; structured errors remain available on JSON calls.
            return Err(LibError::Upstream(format!("HTTP {}", status.as_u16())));
        }
        let content_type = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .map(str::to_string);
        let announced = response.content_length();
        if announced.is_some_and(|length| length > ceiling) {
            return Err(LibError::Unsupported(format!(
                "asset is larger than the {ceiling}-byte preview content cap"
            )));
        }
        let capacity = announced
            .and_then(|length| usize::try_from(length).ok())
            .unwrap_or(0);
        let mut bytes = Vec::with_capacity(capacity);
        let mut stream = response.bytes_stream();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|error| LibError::Upstream(error.to_string()))?;
            let actual = (bytes.len() as u64)
                .checked_add(chunk.len() as u64)
                .ok_or_else(|| LibError::Unsupported("preview content is too large".into()))?;
            if actual > ceiling {
                return Err(LibError::Unsupported(format!(
                    "asset is larger than the {ceiling}-byte preview content cap"
                )));
            }
            bytes.extend_from_slice(&chunk);
        }
        Ok((content_type, bytes))
    }

    pub(crate) fn content_metadata_from_headers(
        headers: &reqwest::header::HeaderMap,
        len: u64,
    ) -> AssetContentMetadata {
        let content_type = headers
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .unwrap_or("application/octet-stream")
            .to_string();
        let (media, format) = media_from_content_type(&content_type);
        let etag = headers
            .get(reqwest::header::ETAG)
            .and_then(|value| value.to_str().ok())
            .map(str::to_string);
        AssetContentMetadata {
            len,
            content_type,
            format,
            media,
            etag,
        }
    }

    pub(crate) fn response_content_len(
        headers: &reqwest::header::HeaderMap,
    ) -> Result<u64, LibError> {
        headers
            .get(reqwest::header::CONTENT_LENGTH)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.parse().ok())
            .ok_or_else(|| LibError::Upstream("content response omitted Content-Length".into()))
    }

    pub(crate) async fn get<T: DeserializeOwned>(&self, path: &str) -> Result<T, LibError> {
        let resp = self
            .http
            .get(self.url(path)?)
            .send()
            .await
            .map_err(|e| LibError::SourceUnavailable(e.to_string()))?;
        Self::decode(resp).await
    }

    pub(crate) async fn post<B: Serialize, T: DeserializeOwned>(
        &self,
        path: &str,
        body: &B,
    ) -> Result<T, LibError> {
        let resp = self
            .http
            .post(self.url(path)?)
            .json(body)
            .send()
            .await
            .map_err(|e| LibError::SourceUnavailable(e.to_string()))?;
        Self::decode(resp).await
    }

    pub(crate) async fn put<B: Serialize, T: DeserializeOwned>(
        &self,
        path: &str,
        body: &B,
    ) -> Result<T, LibError> {
        let resp = self
            .http
            .put(self.url(path)?)
            .json(body)
            .send()
            .await
            .map_err(|e| LibError::SourceUnavailable(e.to_string()))?;
        Self::decode(resp).await
    }

    /// DELETE with no reply body (the common case; a DELETE that answers JSON decodes inline).
    pub(crate) async fn delete(&self, path: &str) -> Result<(), LibError> {
        let resp = self
            .http
            .delete(self.url(path)?)
            .send()
            .await
            .map_err(|e| LibError::SourceUnavailable(e.to_string()))?;
        Self::expect_no_content(resp).await
    }

    // ── federation (phase 6, issue #39) — this client is also the peer transport ──

    /// `GET /api/v1/advertise` — the peer's self-description (protocol version, catalog weight,
    /// embedding spaces). 404 while the peer's `federation` flag is off, which callers surface as
    /// "this instance is not serving federation".
    pub async fn advertise(&self) -> Result<PeerAdvertise, LibError> {
        self.get("/api/v1/advertise").await
    }
}
