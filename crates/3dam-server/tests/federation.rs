//! Federation end-to-end coverage (phase 6, issues #39/#40): a real peer `3dam serve` instance on
//! a real socket, a local embedded engine with a `federated` source pointing at it, and the query
//! fan-out / merge / partial-result semantics frozen in ADR 0009 §5 exercised across the wire.
//!
//! Unlike the phase-5 tests this suite binds sockets: the engine's peer transport is a real HTTP
//! client, so `ServiceExt::oneshot` can't stand in for the peer.

use dam_api::admin::{FlagKey, FlagValue, SetFlag};
use dam_api::dto::*;
use dam_api::event::{LibraryEvent, SubscribeRequest};
use dam_api::page::PageParams;
use dam_api::service::{AuthContext, LibraryService};
use dam_core::EmbeddedLibrary;
use dam_server::{router, McpAdapter, ServerStore, WriteGate};
use futures::StreamExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

fn unique_tmp() -> PathBuf {
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    std::env::temp_dir().join(format!("3dam-fed-{}-{}-{}", std::process::id(), nanos, n))
}

async fn wait_job(lib: &EmbeddedLibrary, ctx: &AuthContext, job: &dam_api::id::JobId) {
    loop {
        let j = lib.get_job(ctx, job).await.unwrap();
        if matches!(
            j.state,
            JobState::Done | JobState::Failed | JobState::Cancelled
        ) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// Open a library over `dir/data`, register `dir/src` as a local source, and scan it.
async fn library_with(dir: &Path, files: &[(&str, &[u8])]) -> Arc<EmbeddedLibrary> {
    let src = dir.join("src");
    std::fs::create_dir_all(&src).unwrap();
    for (name, bytes) in files {
        std::fs::write(src.join(name), bytes).unwrap();
    }
    let lib = Arc::new(
        EmbeddedLibrary::open_with(&dir.join("data"), dam_core::ResourceOptions::ungoverned())
            .await
            .unwrap(),
    );
    let ctx = AuthContext::embedded();
    let sid = lib
        .add_source(
            &ctx,
            AddSource {
                kind: SourceKind::LocalFs,
                uri: src.to_string_lossy().into_owned(),
                name: Some("fixture".into()),
                options: SourceOptions::default(),
            },
        )
        .await
        .unwrap();
    let job = lib
        .submit_scan(
            &ctx,
            ScanRequest {
                sources: vec![sid],
                mode: ScanMode::Full,
            },
        )
        .await
        .unwrap();
    wait_job(&lib, &ctx, &job).await;
    lib
}

/// Serve `lib` as a federation peer on an ephemeral port; returns its endpoint URL.
async fn serve_peer(
    lib: Arc<EmbeddedLibrary>,
) -> (String, std::net::SocketAddr, tokio::task::JoinHandle<()>) {
    let store = Arc::new(ServerStore::open_in_memory().unwrap());
    store
        .set_flag(
            FlagKey::Federation,
            SetFlag {
                value: FlagValue::Bool(true),
                expected_version: None,
                confirm: true,
            },
            "test",
        )
        .unwrap();
    let app = router(lib, store, "127.0.0.1:0", true);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let handle = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (format!("http://{addr}"), addr, handle)
}

/// Register `endpoint` as a federated source on `lib`.
async fn add_peer(lib: &EmbeddedLibrary, endpoint: &str, name: &str) -> dam_api::id::SourceId {
    lib.add_source(
        &AuthContext::embedded(),
        AddSource {
            kind: SourceKind::Federated,
            uri: endpoint.to_string(),
            name: Some(name.to_string()),
            options: SourceOptions::default(),
        },
    )
    .await
    .unwrap()
}

/// A minimal handcrafted HTTP responder: answers **every** request with the given JSON body.
/// Stands in for a peer whose `advertise()` we need to control (wrong protocol / alien space).
async fn fake_peer(body: &'static str) -> (String, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let handle = tokio::spawn(async move {
        loop {
            let Ok((mut sock, _)) = listener.accept().await else {
                break;
            };
            tokio::spawn(async move {
                use tokio::io::{AsyncReadExt, AsyncWriteExt};
                let mut buf = [0u8; 8192];
                let _ = sock.read(&mut buf).await;
                let resp = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len(),
                );
                let _ = sock.write_all(resp.as_bytes()).await;
            });
        }
    });
    (format!("http://{addr}"), handle)
}

/// Tiny federation peer for routing tests. Advertise is always immediate; every other request is
/// counted and either returns a distinct thumbnail byte or deliberately never answers.
async fn thumbnail_peer(
    byte: Option<u8>,
) -> (String, Arc<AtomicUsize>, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let requests = Arc::new(AtomicUsize::new(0));
    let observed = requests.clone();
    let handle = tokio::spawn(async move {
        loop {
            let Ok((mut sock, _)) = listener.accept().await else {
                break;
            };
            let observed = observed.clone();
            tokio::spawn(async move {
                use tokio::io::{AsyncReadExt, AsyncWriteExt};
                let mut buf = [0u8; 8192];
                let read = sock.read(&mut buf).await.unwrap_or(0);
                let request = String::from_utf8_lossy(&buf[..read]);
                if request.starts_with("GET /api/v1/advertise ") {
                    let body = r#"{"protocol_version":"1.0.0","instance":"routing-test","assets":1,"spaces":{}}"#;
                    let response = format!(
                        "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    let _ = sock.write_all(response.as_bytes()).await;
                    return;
                }
                observed.fetch_add(1, Ordering::SeqCst);
                match byte {
                    Some(byte) => {
                        let response = format!(
                            "HTTP/1.1 200 OK\r\ncontent-type: image/png\r\ncontent-length: 1\r\nconnection: close\r\n\r\n{}",
                            char::from(byte)
                        );
                        let _ = sock.write_all(response.as_bytes()).await;
                    }
                    None => std::future::pending::<()>().await,
                }
            });
        }
    });
    (format!("http://{addr}"), requests, handle)
}

fn query_all(limit: u32) -> QueryRequest {
    QueryRequest {
        page: PageParams { after: None, limit },
        ..Default::default()
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn hinted_reads_contact_one_owner_and_peer_caches_do_not_collide() {
    let local = library_with(&unique_tmp(), &[("local.wav", b"RIFF....WAVE")]).await;
    let (a_endpoint, a_requests, _a) = thumbnail_peer(Some(b'A')).await;
    let (b_endpoint, b_requests, _b) = thumbnail_peer(Some(b'B')).await;
    let (offline_endpoint, offline_requests, _offline) = thumbnail_peer(None).await;
    let a_source = add_peer(&local, &a_endpoint, "owner-a").await;
    let b_source = add_peer(&local, &b_endpoint, "owner-b").await;
    let _offline_source = add_peer(&local, &offline_endpoint, "unrelated-offline").await;
    a_requests.store(0, Ordering::SeqCst);
    b_requests.store(0, Ordering::SeqCst);
    offline_requests.store(0, Ordering::SeqCst);
    let (local_endpoint, _local_addr, _local_server) = serve_peer(local.clone()).await;

    // The same remote id can legitimately exist on both peers. Each hinted read goes straight to
    // its owner, and the owner is part of the disk-cache key so the bytes cannot alias.
    let id: dam_api::id::AssetId = "00000000-0000-0000-0000-000000000150".parse().unwrap();
    let started = std::time::Instant::now();
    let a = reqwest::get(format!(
        "{local_endpoint}/api/v1/assets/{id}/thumbnail?edge=256&source={a_source}"
    ))
    .await
    .unwrap()
    .bytes()
    .await
    .unwrap();
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "an unrelated offline peer must not add latency to a hinted read"
    );
    let b = reqwest::get(format!(
        "{local_endpoint}/api/v1/assets/{id}/thumbnail?edge=256&source={b_source}"
    ))
    .await
    .unwrap()
    .bytes()
    .await
    .unwrap();
    assert_eq!(a.as_ref(), b"A");
    assert_eq!(b.as_ref(), b"B");
    assert_eq!(a_requests.load(Ordering::SeqCst), 1);
    assert_eq!(b_requests.load(Ordering::SeqCst), 1);
    assert_eq!(offline_requests.load(Ordering::SeqCst), 0);

    // Both second reads are owner-scoped cache hits: no peer receives another request.
    assert_eq!(
        local
            .read_thumbnail_from(&AuthContext::embedded(), &id, 256, Some(a_source))
            .await
            .unwrap()
            .bytes,
        b"A"
    );
    assert_eq!(
        local
            .read_thumbnail_from(&AuthContext::embedded(), &id, 256, Some(b_source))
            .await
            .unwrap()
            .bytes,
        b"B"
    );
    assert_eq!(a_requests.load(Ordering::SeqCst), 1);
    assert_eq!(b_requests.load(Ordering::SeqCst), 1);
    assert_eq!(offline_requests.load(Ordering::SeqCst), 0);

    // An old bookmark has no owner hint. Recovery is deliberately exceptional and hedges one
    // bounded round across the registry: the hanging peer cannot turn latency into N × timeout.
    let old_id: dam_api::id::AssetId = "00000000-0000-0000-0000-000000000151".parse().unwrap();
    let recovery_started = std::time::Instant::now();
    let recovered = local
        .read_thumbnail_from(&AuthContext::embedded(), &old_id, 256, None)
        .await
        .unwrap();
    assert!(recovered.bytes == b"A" || recovered.bytes == b"B");
    assert!(
        recovery_started.elapsed() < Duration::from_secs(2),
        "legacy recovery latency must not depend on the unrelated hanging peer"
    );

    // Removing the hinted owner invalidates the registry and its cache namespace. The stale hint
    // cannot serve owner A's cached bytes; bounded recovery may find the colliding id on owner B.
    local
        .remove_source(
            &AuthContext::embedded(),
            &a_source,
            RemoveSource {
                keep_metadata: false,
            },
        )
        .await
        .unwrap();
    let after_remove = local
        .read_thumbnail_from(&AuthContext::embedded(), &id, 256, Some(a_source))
        .await
        .unwrap();
    assert_eq!(after_remove.bytes, b"B");
}

// ── merged browse/search (issue #39 acceptance) ──────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn query_merges_local_and_peer_results() {
    let ctx = AuthContext::embedded();
    let peer_lib = library_with(
        &unique_tmp(),
        &[("b.wav", b"RIFF....WAVE"), ("d.png", b"\x89PNG\r\n")],
    )
    .await;
    let (endpoint, _addr, _srv) = serve_peer(peer_lib).await;

    let local = library_with(
        &unique_tmp(),
        &[("a.wav", b"RIFF....WAVE"), ("c.png", b"\x89PNG\r\n")],
    )
    .await;
    add_peer(&local, &endpoint, "studio-server").await;

    let page = local.query(&ctx, query_all(50)).await.unwrap();
    assert!(page.partial.complete, "both sources answered");
    assert_eq!(
        page.total,
        Some(4),
        "exact totals from every answering stream are summed under fan-out"
    );
    let names: Vec<&str> = page.items.iter().map(|a| a.name.as_str()).collect();
    assert_eq!(
        names,
        ["a.wav", "b.wav", "c.png", "d.png"],
        "one page, name-merged across local + peer"
    );
    for item in &page.items {
        let expect_peer = item.name == "b.wav" || item.name == "d.png";
        match &item.origin {
            Origin::Local => assert!(!expect_peer, "{} must be peer-tagged", item.name),
            Origin::Peer(p) => {
                assert!(expect_peer, "{} must be local", item.name);
                assert_eq!(p, "studio-server");
            }
        }
    }

    // Cursor-walk the same listing one item at a time: same set, same order, no dupes.
    let mut walked = Vec::new();
    let mut after = None;
    loop {
        let mut req = query_all(1);
        req.page.after = after;
        let page = local.query(&ctx, req).await.unwrap();
        walked.extend(page.items.iter().map(|a| a.name.clone()));
        match page.cursor {
            Some(c) => after = Some(c),
            None => break,
        }
        assert!(walked.len() <= 8, "cursor must terminate");
    }
    assert_eq!(walked, ["a.wav", "b.wav", "c.png", "d.png"]);

    // A peer asset's detail read proxies through to the owning peer, origin re-tagged.
    let b = page.items.iter().find(|a| a.name == "b.wav").unwrap();
    let detail = local
        .get_asset_from(&ctx, &b.id, b.source_id)
        .await
        .unwrap();
    assert_eq!(detail.summary.name, "b.wav");
    assert!(matches!(detail.summary.origin, Origin::Peer(ref p) if p == "studio-server"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn source_filter_routes_to_the_peer_alone() {
    let ctx = AuthContext::embedded();
    let peer_lib = library_with(&unique_tmp(), &[("remote.wav", b"RIFF....WAVE")]).await;
    let (endpoint, _addr, _srv) = serve_peer(peer_lib).await;
    let local = library_with(&unique_tmp(), &[("local.wav", b"RIFF....WAVE")]).await;
    let fed_sid = add_peer(&local, &endpoint, "peer").await;

    let mut req = query_all(50);
    req.filters.push(Filter {
        field: FacetField::Source,
        op: FilterOp::Eq,
        value: FilterValue::Str(fed_sid.to_string()),
    });
    let page = local.query(&ctx, req).await.unwrap();
    let names: Vec<&str> = page.items.iter().map(|a| a.name.as_str()).collect();
    assert_eq!(
        names,
        ["remote.wav"],
        "selecting the federated source browses the peer's catalog"
    );

    // Stats scoped to the peer source come from the peer itself, live — the sidebar counts show
    // the peer's library, not the local catalog (which holds zero of its rows).
    let peer_stats = local.library_stats(&ctx, Some(fed_sid)).await.unwrap();
    assert_eq!(peer_stats.total, 1, "the peer's own total");
    assert_eq!(peer_stats.by_media.get("audio"), Some(&1));

    // Scoped to the local source: only the local folder's rows.
    let local_sid = local
        .list_sources(&ctx)
        .await
        .unwrap()
        .into_iter()
        .find(|s| s.kind == SourceKind::LocalFs)
        .unwrap()
        .id;
    let local_stats = local.library_stats(&ctx, Some(local_sid)).await.unwrap();
    assert_eq!(local_stats.total, 1);
    // Unscoped stays the local library (peers merge into queries, not local aggregates).
    let all = local.library_stats(&ctx, None).await.unwrap();
    assert_eq!(all.total, 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn restricted_federated_share_reaches_only_that_peer_and_revokes_cached_reads() {
    let owner = AuthContext::embedded();

    let peer_a_dir = unique_tmp();
    std::fs::create_dir_all(peer_a_dir.join("src")).unwrap();
    gradient(32, 32)
        .save(peer_a_dir.join("src").join("a-reference.png"))
        .unwrap();
    let mut near = gradient(32, 32);
    near.put_pixel(0, 0, image::Rgba([250, 1, 1, 255]));
    near.save(peer_a_dir.join("src").join("a-near.png"))
        .unwrap();
    let peer_a = library_with(&peer_a_dir, &[]).await;
    analyze_all(&peer_a, &owner).await;
    let (endpoint_a, _addr_a, _server_a) = serve_peer(peer_a).await;

    let peer_b_dir = unique_tmp();
    std::fs::create_dir_all(peer_b_dir.join("src")).unwrap();
    let mut secret = gradient(32, 32);
    secret.put_pixel(3, 7, image::Rgba([1, 250, 1, 255]));
    secret
        .save(peer_b_dir.join("src").join("b-secret.png"))
        .unwrap();
    let peer_b = library_with(&peer_b_dir, &[]).await;
    analyze_all(&peer_b, &owner).await;
    let (endpoint_b, _addr_b, _server_b) = serve_peer(peer_b).await;

    let local = library_with(&unique_tmp(), &[("local.wav", b"RIFF....WAVE")]).await;
    let source_a = add_peer(&local, &endpoint_a, "shared-peer").await;
    let source_b = add_peer(&local, &endpoint_b, "hidden-peer").await;

    let all = local.query(&owner, query_all(50)).await.unwrap();
    let reference = all
        .items
        .iter()
        .find(|asset| asset.name == "a-reference.png")
        .unwrap()
        .clone();
    let hidden = all
        .items
        .iter()
        .find(|asset| asset.name == "b-secret.png")
        .unwrap()
        .clone();
    assert_ne!(reference.id, hidden.id, "fixtures must not alias asset ids");

    let mut scope = dam_api::VisibilityScope::default();
    scope.sources.insert(source_a);
    let restricted = AuthContext::connected(
        Some("shared-peer-viewer".into()),
        dam_api::service::Scopes::anonymous(),
        dam_api::Visibility::Restricted(scope),
    );

    // Search fans out to exactly the granted peer: neither the local index nor another peer may
    // contribute rows or warnings that disclose it was contacted.
    let page = local.query(&restricted, query_all(50)).await.unwrap();
    let names: Vec<&str> = page.items.iter().map(|asset| asset.name.as_str()).collect();
    assert_eq!(names, ["a-near.png", "a-reference.png"]);
    assert!(page.partial.complete);
    assert!(page
        .items
        .iter()
        .all(|asset| asset.source_id == Some(source_a)));

    let detail = local
        .get_asset_from(&restricted, &reference.id, Some(source_a))
        .await
        .unwrap();
    assert_eq!(detail.summary.name, "a-reference.png");
    assert!(matches!(detail.summary.origin, Origin::Peer(ref name) if name == "shared-peer"));
    assert!(!local
        .read_content_from(&restricted, &reference.id, Some(source_a))
        .await
        .unwrap()
        .bytes
        .is_empty());
    // Populate the outer peer cache before the revocation check below.
    assert!(!local
        .read_thumbnail_from(&restricted, &reference.id, 128, Some(source_a))
        .await
        .unwrap()
        .bytes
        .is_empty());

    // A wrong, missing, or unshared hint cannot trigger the unrestricted recovery round.
    assert!(local
        .get_asset_from(&restricted, &hidden.id, Some(source_b))
        .await
        .is_err());
    assert!(local
        .get_asset_from(&restricted, &reference.id, None)
        .await
        .is_err());
    assert!(local
        .read_thumbnail_from(&restricted, &hidden.id, 128, Some(source_b))
        .await
        .is_err());

    let scoped = local
        .library_stats(&restricted, Some(source_a))
        .await
        .unwrap();
    assert_eq!(scoped.total, 2);
    let aggregate = local.library_stats(&restricted, None).await.unwrap();
    assert_eq!(aggregate.total, 2);
    assert_eq!(aggregate.sources, 1);
    assert_eq!(aggregate.by_source.get("shared-peer"), Some(&2));
    assert!(!aggregate.by_source.contains_key("hidden-peer"));

    let mcp = McpAdapter::new(local.clone(), WriteGate::local_stdio());
    let reply = mcp
        .handle_message(
            &restricted,
            serde_json::json!({
                "jsonrpc": "2.0", "id": 1, "method": "tools/call",
                "params": {"name": "search", "arguments": {"limit": 50}}
            }),
        )
        .await
        .unwrap();
    let text = reply["result"]["content"][0]["text"].as_str().unwrap();
    assert!(text.contains("a-reference.png"));
    assert!(!text.contains("b-secret.png"));
    assert!(!text.contains("local.wav"));

    let export_path = unique_tmp().join("shared-peer.json");
    let report = local
        .export(
            &restricted,
            ExportRequest {
                assets: Vec::new(),
                collection: None,
                query: None,
                format: ExportFormat::Json,
                output: export_path.to_string_lossy().into_owned(),
                attribution_only: false,
            },
        )
        .await
        .unwrap();
    assert_eq!(report.assets, 2);
    let manifest: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&export_path).unwrap()).unwrap();
    let exported: Vec<&str> = manifest["assets"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|asset| asset["name"].as_str())
        .collect();
    assert_eq!(exported, ["a-near.png", "a-reference.png"]);

    // Background export jobs and their live progress events carry the same one-peer attribution.
    let mut events = local
        .subscribe(&restricted, SubscribeRequest::default())
        .await
        .unwrap();
    let job = local
        .submit_export(
            &restricted,
            ExportRequest {
                assets: Vec::new(),
                collection: None,
                query: None,
                format: ExportFormat::Json,
                output: unique_tmp().join("job.json").to_string_lossy().into_owned(),
                attribution_only: false,
            },
        )
        .await
        .unwrap();
    let event = tokio::time::timeout(Duration::from_secs(10), events.next())
        .await
        .unwrap()
        .unwrap();
    match event {
        LibraryEvent::JobProgress(progress) => {
            assert_eq!(progress.id, job);
            assert_eq!(progress.sources, vec![source_a]);
        }
        other => panic!("expected shared-peer export progress, got {other:?}"),
    }
    wait_job(&local, &restricted, &job).await;

    let similar = local
        .find_similar(
            &restricted,
            SimilarRequest {
                asset: reference.id,
                k: 10,
                filters: Vec::new(),
                local_only: false,
            },
        )
        .await
        .unwrap();
    assert!(similar
        .items
        .iter()
        .any(|hit| hit.asset.name == "a-near.png" && hit.asset.source_id == Some(source_a)));
    assert!(local
        .find_similar(
            &restricted,
            SimilarRequest {
                asset: hidden.id,
                k: 10,
                filters: Vec::new(),
                local_only: false,
            },
        )
        .await
        .is_err());

    // Remote ownership stays authoritative even if an administrator attempted to describe the
    // source share as writable: no local asset row exists to mutate.
    assert!(local
        .set_favorite(
            &restricted,
            FavoriteRequest {
                asset: reference.id,
                favorite: true,
            },
        )
        .await
        .is_err());

    // The next request after revocation cannot use an already-populated preview cache.
    let revoked = AuthContext::connected(
        Some("shared-peer-viewer".into()),
        dam_api::service::Scopes::anonymous(),
        dam_api::Visibility::Restricted(dam_api::VisibilityScope::default()),
    );
    assert!(local
        .query(&revoked, query_all(50))
        .await
        .unwrap()
        .items
        .is_empty());
    assert!(local
        .read_thumbnail_from(&revoked, &reference.id, 128, Some(source_a))
        .await
        .is_err());
}

// ── partial results on a dead/slow peer (issue #39 acceptance) ───────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn hung_peer_is_dropped_at_the_deadline_and_flagged_partial() {
    let ctx = AuthContext::embedded();
    let peer_lib = library_with(&unique_tmp(), &[("remote.wav", b"RIFF....WAVE")]).await;
    let (endpoint, addr, srv) = serve_peer(peer_lib).await;
    let local = library_with(&unique_tmp(), &[("local.wav", b"RIFF....WAVE")]).await;
    add_peer(&local, &endpoint, "peer").await;

    // Replace the healthy peer with one that accepts connections but never answers: the fan-out
    // must return the local page at the 2.5 s deadline, not hang on the slowest peer.
    srv.abort();
    let _ = tokio::time::timeout(Duration::from_secs(2), async {
        while tokio::net::TcpListener::bind(addr).await.is_err() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await;
    let hang = tokio::net::TcpListener::bind(addr).await.unwrap();
    let _hang_task = tokio::spawn(async move {
        let mut held = Vec::new();
        while let Ok((sock, _)) = hang.accept().await {
            held.push(sock); // accept and hold — never respond
        }
    });

    let started = std::time::Instant::now();
    let page = local.query(&ctx, query_all(50)).await.unwrap();
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "the deadline bounds the round"
    );
    let names: Vec<&str> = page.items.iter().map(|a| a.name.as_str()).collect();
    assert_eq!(names, ["local.wav"], "local results survive a dead peer");
    assert!(!page.partial.complete, "the dropped peer flags the page");
    assert!(
        page.partial
            .warnings
            .iter()
            .any(|w| w.code == "peer_dropped"),
        "warnings name the dropped peer: {:?}",
        page.partial.warnings
    );
}

// ── protocol version negotiation (issue #39 acceptance) ──────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn mismatched_protocol_version_is_rejected_at_add() {
    let (endpoint, _srv) =
        fake_peer(r#"{"protocol_version":"2.0.0","instance":"future","assets":0,"spaces":{}}"#)
            .await;
    let local = library_with(&unique_tmp(), &[("local.wav", b"RIFF....WAVE")]).await;
    let err = local
        .add_source(
            &AuthContext::embedded(),
            AddSource {
                kind: SourceKind::Federated,
                uri: endpoint,
                name: Some("future-peer".into()),
                options: SourceOptions::default(),
            },
        )
        .await
        .unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("federation protocol"),
        "the rejection names the version mismatch: {msg}"
    );
}

// ── cross-peer similarity: the embedding-space gate (issue #40) ───────────────

fn gradient(w: u32, h: u32) -> image::RgbaImage {
    image::RgbaImage::from_fn(w, h, |x, y| {
        image::Rgba([(x * 4 % 256) as u8, (y * 4 % 256) as u8, 128, 255])
    })
}

async fn analyze_all(lib: &EmbeddedLibrary, ctx: &AuthContext) {
    let job = lib
        .submit_analyze(ctx, AnalyzeRequest::default())
        .await
        .unwrap();
    wait_job(lib, ctx, &job).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn similar_merges_matched_space_peers_and_gates_mismatched_spaces() {
    let ctx = AuthContext::embedded();

    // Peer: a near-copy of the local reference image, analysed so it has an embedding.
    let peer_dir = unique_tmp();
    std::fs::create_dir_all(peer_dir.join("src")).unwrap();
    let mut near = gradient(64, 64);
    for i in 0..3u32 {
        near.put_pixel(i, i, image::Rgba([200, 10, 10, 255]));
    }
    near.save(peer_dir.join("src").join("remote_near.png"))
        .unwrap();
    let peer_lib = library_with(&peer_dir, &[]).await;
    analyze_all(&peer_lib, &ctx).await;
    let (endpoint, _addr, _srv) = serve_peer(peer_lib.clone()).await;

    // Local: the reference image, analysed.
    let local_dir = unique_tmp();
    std::fs::create_dir_all(local_dir.join("src")).unwrap();
    gradient(64, 64)
        .save(local_dir.join("src").join("reference.png"))
        .unwrap();
    let local = library_with(&local_dir, &[]).await;
    analyze_all(&local, &ctx).await;
    add_peer(&local, &endpoint, "studio-server").await;

    let reference = local
        .query(&ctx, query_all(10))
        .await
        .unwrap()
        .items
        .into_iter()
        .find(|a| a.name == "reference.png" && matches!(a.origin, Origin::Local))
        .unwrap();

    // Matched space (both rank in image-stats-v1): the peer's neighbour joins one ranked list.
    let hits = local
        .find_similar(
            &ctx,
            SimilarRequest {
                asset: reference.id,
                k: 10,
                filters: Vec::new(),
                local_only: false,
            },
        )
        .await
        .unwrap();
    let peer_hit = hits
        .items
        .iter()
        .find(|h| matches!(h.asset.origin, Origin::Peer(_)))
        .expect("the matched-space peer contributes to the unified ranking");
    assert_eq!(peer_hit.asset.name, "remote_near.png");
    assert_eq!(peer_hit.space, "image-stats-v1");

    // The serving side rejects a vector in a space it doesn't hold — the exact-match gate never
    // silently compares distances across spaces (issue #40 acceptance).
    let err = peer_lib
        .find_similar_by_vector(
            &ctx,
            dam_api::VectorSimilarRequest {
                media: MediaType::Image,
                space: "someone-elses-space@9".into(),
                vector: vec![0.5; 8],
                k: 5,
                filters: Vec::new(),
            },
        )
        .await
        .unwrap_err();
    assert!(matches!(err, dam_api::LibError::BadRequest(_)));

    // A peer advertising an alien space is omitted from the unified ranking and flagged — its
    // hits are never co-ranked against local cosines.
    let (alien_endpoint, _alien) = fake_peer(
        r#"{"protocol_version":"1.0.0","instance":"alien","assets":1,"spaces":{"image":"alien-clip@2"}}"#,
    )
    .await;
    add_peer(&local, &alien_endpoint, "alien-peer").await;
    let gated = local
        .find_similar(
            &ctx,
            SimilarRequest {
                asset: reference.id,
                k: 10,
                filters: Vec::new(),
                local_only: false,
            },
        )
        .await
        .unwrap();
    assert!(
        !gated
            .items
            .iter()
            .any(|h| matches!(h.asset.origin, Origin::Peer(ref p) if p == "alien-peer")),
        "a mismatched-space peer never co-ranks"
    );
    assert!(
        gated
            .partial
            .warnings
            .iter()
            .any(|w| w.code == "space_mismatch" && w.subject == "alien-peer"),
        "the omission is flagged, not silent: {:?}",
        gated.partial.warnings
    );
}

/// Rights edits stop at the library boundary (issue #106; PRODUCT_SPEC §5 and tech-spec 02 §4:
/// federated assets are read-only and origin-attributed).
///
/// The guard is **structural**, not a check the write path performs: federation *proxies* reads
/// rather than mirroring rows, so a peer-owned id is never in the borrowing library's `asset` table
/// and no local write can resolve it. `set_license` therefore resolves it to nothing and reports
/// `target_unavailable` — not `target_read_only`, because from the local catalog's point of view
/// the row genuinely is not there. This test pins that: the peer's own rights must be untouched.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_peer_asset_cannot_be_relicensed_from_the_borrowing_library() {
    let ctx = AuthContext::embedded();
    let peer_lib = library_with(&unique_tmp(), &[("remote.png", b"\x89PNG\r\n")]).await;
    let (endpoint, _addr, _srv) = serve_peer(peer_lib.clone()).await;

    let local = library_with(&unique_tmp(), &[("local.png", b"\x89PNG\r\n")]).await;
    add_peer(&local, &endpoint, "studio-server").await;

    let page = local.query(&ctx, query_all(50)).await.unwrap();
    let remote = page
        .items
        .iter()
        .find(|a| a.name == "remote.png")
        .expect("the peer's asset is visible in the merged page");
    assert!(matches!(remote.origin, Origin::Peer(ref p) if p == "studio-server"));

    let result = local
        .set_license(
            &ctx,
            SetLicenseRequest {
                assets: vec![remote.id],
                license: LicenseInput {
                    id: Some(Some("CC0-1.0".into())),
                    commercial: Some(Some(true)),
                    modify: Some(Some(true)),
                    redistribute: Some(Some(true)),
                    attribution: Some(Some(false)),
                    ..Default::default()
                },
                ..Default::default()
            },
        )
        .await
        .unwrap();

    assert_eq!(result.matched, 0, "a peer asset is not a writable target");
    assert_eq!(result.changed, 0);
    assert_eq!(
        result
            .warnings
            .iter()
            .map(|w| w.code.as_str())
            .collect::<Vec<_>>(),
        ["target_unavailable"],
        "the refusal is reported, not silent: {:?}",
        result.warnings
    );

    // The owning library is the only place that licence could have been written, and it wasn't.
    assert_eq!(
        peer_lib
            .get_asset(&ctx, &remote.id)
            .await
            .unwrap()
            .license
            .status,
        LicenseStatus::Unknown,
        "a borrowing library must never rewrite a peer's rights"
    );
    // And the borrowed view still reads through to the peer's own (unchanged) block.
    let proxied = local
        .get_asset_from(&ctx, &remote.id, remote.source_id)
        .await
        .unwrap();
    assert_eq!(proxied.license.id, None);
    assert!(matches!(proxied.summary.origin, Origin::Peer(_)));
}
