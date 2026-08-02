//! Runs one normalized behavior transcript through both `LibraryService` implementations. IDs and
//! timestamps intentionally stay out of the transcript; observable semantics must match.

use dam_api::admin::{AuthMode, FlagKey, FlagValue, NewToken, SetFlag};
use dam_api::dto::*;
use dam_api::page::PageParams;
use dam_api::service::{AuthContext, LibraryService, Scope, Scopes};
use dam_client::ApiClient;
use dam_core::EmbeddedLibrary;
use dam_server::{router, ServerStore};
use futures::StreamExt;
use std::sync::Arc;
use std::time::Duration;

#[derive(Debug, PartialEq, Eq)]
struct Transcript {
    initial_sources: usize,
    source: (String, SourceKind),
    scan: (JobKind, JobState),
    asset: (String, MediaType, String, usize),
    totals: (u64, u64),
    favorite: bool,
    note: Option<String>,
    collection: (String, u64, Vec<String>),
    similar: usize,
    duplicates: usize,
    missing_code: String,
    final_sources: usize,
}

async fn wait_for_job(
    service: &dyn LibraryService,
    ctx: &AuthContext,
    id: &dam_api::JobId,
) -> JobStatus {
    for _ in 0..500 {
        let job = service.get_job(ctx, id).await.unwrap();
        if matches!(
            job.state,
            JobState::Done | JobState::Failed | JobState::Cancelled
        ) {
            return job;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("scan job did not terminate");
}

async fn behavior_transcript(
    service: &dyn LibraryService,
    ctx: &AuthContext,
    source_path: &std::path::Path,
) -> Transcript {
    let who = service.whoami(ctx).await.unwrap();
    assert!(who.scopes.has(dam_api::service::Scope::Read));
    assert!(who.scopes.has(dam_api::service::Scope::Write));

    let initial_sources = service.list_sources(ctx).await.unwrap().len();
    let source_id = service
        .add_source(
            ctx,
            AddSource {
                kind: SourceKind::LocalFs,
                uri: source_path.to_string_lossy().into_owned(),
                name: Some("Contract samples".into()),
                options: SourceOptions::default(),
            },
        )
        .await
        .unwrap();
    let source = service.get_source(ctx, &source_id).await.unwrap();
    assert_eq!(service.list_sources(ctx).await.unwrap().len(), 1);

    let job_id = service
        .submit_scan(
            ctx,
            ScanRequest {
                sources: vec![source_id],
                mode: ScanMode::Full,
            },
        )
        .await
        .unwrap();
    let job = wait_for_job(service, ctx, &job_id).await;
    assert_eq!(job.state, JobState::Done, "scan failed: {:?}", job.error);
    assert!(service
        .list_jobs(ctx, JobListRequest::default())
        .await
        .unwrap()
        .items
        .iter()
        .any(|j| j.id == job_id));

    let page = service.query(ctx, QueryRequest::default()).await.unwrap();
    assert_eq!(page.items.len(), 1);
    let summary = page.items[0].clone();
    let asset = service.get_asset(ctx, &summary.id).await.unwrap();
    let metadata = service.content_metadata(ctx, &summary.id).await.unwrap();
    let content = service.read_content(ctx, &summary.id).await.unwrap();
    let stats = service.library_stats(ctx, None).await.unwrap();

    service
        .set_favorite(
            ctx,
            FavoriteRequest {
                asset: summary.id,
                favorite: true,
            },
        )
        .await
        .unwrap();
    let favorite = service
        .get_asset(ctx, &summary.id)
        .await
        .unwrap()
        .summary
        .favorite;
    let note = service
        .set_note(
            ctx,
            &summary.id,
            NoteRequest {
                body: "parity note".into(),
            },
        )
        .await
        .unwrap();
    assert_eq!(
        service
            .get_note(ctx, &summary.id)
            .await
            .unwrap()
            .as_ref()
            .map(|n| n.body.as_str()),
        Some("parity note")
    );

    let collection_id = service
        .create_collection(
            ctx,
            NewCollection {
                name: "Parity collection".into(),
                kind: CollectionKind::Manual,
                query: None,
            },
        )
        .await
        .unwrap();
    service
        .modify_collection_members(
            ctx,
            &collection_id,
            CollectionMembers {
                add: vec![summary.id],
                remove: vec![],
            },
        )
        .await
        .unwrap();
    service
        .update_collection(
            ctx,
            &collection_id,
            UpdateCollection {
                name: Some("Parity collection renamed".into()),
                query: None,
            },
        )
        .await
        .unwrap();
    let collection = service.get_collection(ctx, &collection_id).await.unwrap();
    let collection_assets = service
        .collection_assets(ctx, &collection_id, PageParams::default())
        .await
        .unwrap();
    assert!(service
        .list_collections(ctx)
        .await
        .unwrap()
        .iter()
        .any(|c| c.id == collection_id));

    let similar = service
        .find_similar(
            ctx,
            SimilarRequest {
                asset: summary.id,
                k: 5,
                filters: vec![],
                local_only: true,
            },
        )
        .await
        .unwrap();
    let duplicates = service
        .list_duplicates(ctx, DupRequest::default())
        .await
        .unwrap();
    let missing_code = service
        .get_asset(ctx, &dam_api::AssetId::new())
        .await
        .unwrap_err()
        .code()
        .to_string();

    service
        .delete_collection(ctx, &collection_id)
        .await
        .unwrap();
    service
        .remove_source(
            ctx,
            &source_id,
            RemoveSource {
                keep_metadata: false,
            },
        )
        .await
        .unwrap();

    Transcript {
        initial_sources,
        source: (source.name, source.kind),
        scan: (job.kind, job.state),
        asset: (
            asset.summary.name,
            asset.summary.media,
            metadata.content_type,
            content.bytes.len(),
        ),
        totals: (stats.total, stats.sources),
        favorite,
        note: note.map(|n| n.body),
        collection: (
            collection.name,
            collection.count.unwrap_or(0),
            collection_assets
                .items
                .into_iter()
                .map(|a| a.name)
                .collect(),
        ),
        similar: similar.items.len(),
        duplicates: duplicates.items.len(),
        missing_code,
        final_sources: service.list_sources(ctx).await.unwrap().len(),
    }
}

async fn assert_connected_read_only_outcome(
    service: &dyn LibraryService,
    ctx: &AuthContext,
    source_path: &std::path::Path,
) {
    let who = service.whoami(ctx).await.unwrap();
    assert!(who.scopes.has(Scope::Read));
    assert!(!who.scopes.has(Scope::Write));
    service.library_stats(ctx, None).await.unwrap();
    let error = service
        .add_source(
            ctx,
            AddSource {
                kind: SourceKind::LocalFs,
                uri: source_path.to_string_lossy().into_owned(),
                name: Some("must be rejected".into()),
                options: SourceOptions::default(),
            },
        )
        .await
        .unwrap_err();
    assert_eq!(error.code(), "forbidden");
}

fn sample_root(parent: &std::path::Path, name: &str) -> std::path::PathBuf {
    let root = parent.join(name);
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(root.join("kick.wav"), b"RIFF....WAVE").unwrap();
    root
}

#[tokio::test]
async fn embedded_and_connected_services_have_the_same_core_semantics() {
    let temp = tempfile::tempdir().unwrap();
    let embedded = EmbeddedLibrary::open_with(
        &temp.path().join("embedded-data"),
        dam_core::ResourceOptions::ungoverned(),
    )
    .await
    .unwrap();
    let embedded_result = behavior_transcript(
        &embedded,
        &AuthContext::embedded(),
        &sample_root(temp.path(), "embedded-source"),
    )
    .await;

    let served_library = Arc::new(
        EmbeddedLibrary::open_with(
            &temp.path().join("served-data"),
            dam_core::ResourceOptions::ungoverned(),
        )
        .await
        .unwrap(),
    );
    let store = Arc::new(ServerStore::open_in_memory().unwrap());
    let app = router(served_library, store.clone(), "127.0.0.1:0", true);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let client = ApiClient::connect(format!("http://{address}").parse().unwrap())
        .await
        .unwrap();
    let connected_result = behavior_transcript(
        &client,
        &AuthContext::embedded(),
        &sample_root(temp.path(), "connected-source"),
    )
    .await;

    // Embedded mode is deliberately full-trust: there is no credential boundary in-process.
    // Scope enforcement is therefore characterized at the real connected boundary below.
    let token = store
        .create_token(
            NewToken {
                label: "reader".into(),
                scopes: Scopes::none().with(Scope::Read),
                expires: None,
            },
            "test",
        )
        .unwrap();
    store
        .set_flag(
            FlagKey::Authentication,
            SetFlag {
                value: FlagValue::Auth(AuthMode::Token),
                expected_version: None,
                confirm: false,
            },
            "test",
        )
        .unwrap();
    let read_client = ApiClient::connect_with_token(
        format!("http://{address}").parse().unwrap(),
        Some(token.secret),
    )
    .await
    .unwrap();
    assert_connected_read_only_outcome(
        &read_client,
        &AuthContext::embedded(),
        &sample_root(temp.path(), "connected-forbidden-source"),
    )
    .await;
    server.abort();

    assert_eq!(embedded_result, connected_result);
}

#[tokio::test]
async fn token_authenticated_real_server_subscription_uses_ticket_and_filters_topics() {
    let temp = tempfile::tempdir().unwrap();
    let library = Arc::new(
        EmbeddedLibrary::open_with(
            &temp.path().join("ws-data"),
            dam_core::ResourceOptions::ungoverned(),
        )
        .await
        .unwrap(),
    );
    let store = Arc::new(ServerStore::open_in_memory().unwrap());
    let token = store
        .create_token(
            NewToken {
                label: "ws-test".into(),
                scopes: Scopes::owner(),
                expires: None,
            },
            "test",
        )
        .unwrap();
    store
        .set_flag(
            FlagKey::Authentication,
            SetFlag {
                value: FlagValue::Auth(AuthMode::Token),
                expected_version: None,
                confirm: false,
            },
            "test",
        )
        .unwrap();
    let app = router(library, store, "127.0.0.1:0", true);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let client = ApiClient::connect_with_token(
        format!("http://{address}").parse().unwrap(),
        Some(token.secret),
    )
    .await
    .unwrap();
    let mut events = client
        .subscribe(
            &AuthContext::embedded(),
            dam_api::event::SubscribeRequest {
                topics: vec![dam_api::event::EventTopic::Jobs],
            },
        )
        .await
        .unwrap();
    // `subscribe` returns the bounded receiver immediately while its socket task mints the ticket;
    // give the local handshake time to complete before emitting the one event under test.
    tokio::time::sleep(Duration::from_millis(500)).await;

    let source_id = client
        .add_source(
            &AuthContext::embedded(),
            AddSource {
                kind: SourceKind::LocalFs,
                uri: sample_root(temp.path(), "ws-source")
                    .to_string_lossy()
                    .into_owned(),
                name: Some("WS source".into()),
                options: SourceOptions::default(),
            },
        )
        .await
        .unwrap();
    let job_id = client
        .submit_scan(
            &AuthContext::embedded(),
            ScanRequest {
                sources: vec![source_id],
                mode: ScanMode::Full,
            },
        )
        .await
        .unwrap();

    let event = tokio::time::timeout(Duration::from_secs(5), events.next())
        .await
        .expect("job event should arrive through the ticketed real-server socket")
        .expect("subscription should remain open");
    assert!(matches!(
        event,
        dam_api::event::LibraryEvent::JobProgress(job) if job.id == job_id
    ));
    server.abort();
}
