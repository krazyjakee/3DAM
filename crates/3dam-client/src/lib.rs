//! `dam-client` — the API-client `LibraryService`. Implements the *same* trait as the embedded
//! engine by calling a remote `3dam serve` over `/api/v1` (tech-spec 03 §8). A front-end holding a
//! `Box<dyn LibraryService>` cannot tell this from the in-process engine — that is the whole point
//! of the seam. Phase 1 covers the REST slice; the WS live-update transport lands with the server WS.

use async_trait::async_trait;
use dam_api::dto::*;
use dam_api::event::{LibraryEvent, SubscribeRequest};
use dam_api::id::{AssetId, JobId, SourceId};
use dam_api::page::Page;
use dam_api::service::{AuthContext, EventStream, LibraryService};
use dam_api::{ErrorBody, LibError};
use serde::de::DeserializeOwned;
use serde::Serialize;
use url::Url;

#[derive(serde::Deserialize)]
struct IdReply {
    id: SourceId,
}
#[derive(serde::Deserialize)]
struct JobIdReply {
    job_id: JobId,
}

pub struct ApiClient {
    base: Url,
    http: reqwest::Client,
}

impl ApiClient {
    /// Connect to a remote server. `endpoint` is its base URL (e.g. `http://127.0.0.1:7878`).
    pub async fn connect(endpoint: Url) -> Result<ApiClient, LibError> {
        let http = reqwest::Client::builder()
            .build()
            .map_err(|e| LibError::Internal(e.to_string()))?;
        Ok(ApiClient {
            base: endpoint,
            http,
        })
    }

    fn url(&self, path: &str) -> Result<Url, LibError> {
        self.base
            .join(path)
            .map_err(|e| LibError::BadRequest(e.to_string()))
    }

    async fn decode<T: DeserializeOwned>(resp: reqwest::Response) -> Result<T, LibError> {
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

    async fn expect_no_content(resp: reqwest::Response) -> Result<(), LibError> {
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

    async fn get<T: DeserializeOwned>(&self, path: &str) -> Result<T, LibError> {
        let resp = self
            .http
            .get(self.url(path)?)
            .send()
            .await
            .map_err(|e| LibError::SourceUnavailable(e.to_string()))?;
        Self::decode(resp).await
    }

    async fn post<B: Serialize, T: DeserializeOwned>(
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
}

#[async_trait]
impl LibraryService for ApiClient {
    async fn query(
        &self,
        _ctx: &AuthContext,
        req: QueryRequest,
    ) -> Result<Page<AssetSummary>, LibError> {
        self.post("/api/v1/query", &req).await
    }

    async fn get_asset(&self, _ctx: &AuthContext, id: &AssetId) -> Result<Asset, LibError> {
        self.get(&format!("/api/v1/assets/{id}")).await
    }

    async fn library_stats(&self, _ctx: &AuthContext) -> Result<LibraryStats, LibError> {
        self.get("/api/v1/stats").await
    }

    async fn list_sources(&self, _ctx: &AuthContext) -> Result<Vec<SourceInfo>, LibError> {
        self.get("/api/v1/sources").await
    }

    async fn get_source(
        &self,
        _ctx: &AuthContext,
        id: &SourceId,
    ) -> Result<SourceInfo, LibError> {
        self.get(&format!("/api/v1/sources/{id}")).await
    }

    async fn add_source(&self, _ctx: &AuthContext, req: AddSource) -> Result<SourceId, LibError> {
        let reply: IdReply = self.post("/api/v1/sources", &req).await?;
        Ok(reply.id)
    }

    async fn remove_source(
        &self,
        _ctx: &AuthContext,
        id: &SourceId,
        req: RemoveSource,
    ) -> Result<(), LibError> {
        let resp = self
            .http
            .delete(self.url(&format!("/api/v1/sources/{id}"))?)
            .json(&req)
            .send()
            .await
            .map_err(|e| LibError::SourceUnavailable(e.to_string()))?;
        Self::expect_no_content(resp).await
    }

    async fn submit_scan(&self, _ctx: &AuthContext, req: ScanRequest) -> Result<JobId, LibError> {
        let reply: JobIdReply = self.post("/api/v1/jobs/scan", &req).await?;
        Ok(reply.job_id)
    }

    async fn get_job(&self, _ctx: &AuthContext, id: &JobId) -> Result<JobStatus, LibError> {
        self.get(&format!("/api/v1/jobs/{id}")).await
    }

    async fn list_jobs(
        &self,
        _ctx: &AuthContext,
        req: JobListRequest,
    ) -> Result<Page<JobStatus>, LibError> {
        self.post("/api/v1/jobs/list", &req).await
    }

    async fn cancel_job(&self, _ctx: &AuthContext, id: &JobId) -> Result<(), LibError> {
        let resp = self
            .http
            .post(self.url(&format!("/api/v1/jobs/{id}/cancel"))?)
            .send()
            .await
            .map_err(|e| LibError::SourceUnavailable(e.to_string()))?;
        Self::expect_no_content(resp).await
    }

    async fn subscribe(
        &self,
        _ctx: &AuthContext,
        _req: SubscribeRequest,
    ) -> Result<EventStream<LibraryEvent>, LibError> {
        // The WS live-update transport lands with the server WS endpoint; not yet wired client-side.
        Err(LibError::Unsupported(
            "live subscribe over the API client is not implemented yet".into(),
        ))
    }
}
