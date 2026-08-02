//! Integration test for `ApiClient::subscribe` (issue #36): an event emitted "server-side" over the
//! WebSocket firehose must reach an `ApiClient` subscriber as a `LibraryEvent`. A minimal in-test WS
//! server stands in for `3dam serve`'s `/api/v1/ws` — it does exactly what `ws_loop` does (send each
//! event JSON-serialized as a text frame), so this exercises the real client transport end to end.
//!
//! NB: every variant used here is *struct*-shaped. A newtype-with-scalar variant cannot be
//! serialized under the enum's internally-tagged `#[serde(tag = "type")]` representation — it is
//! silently dropped by the server's `ws_loop`. `AssetRemoved` used to be one (`AssetRemoved(AssetId)`)
//! and stopped being one when it grew its `source_id` attribution (issue #42), which incidentally
//! made removals deliverable at all; `assets_removed_round_trips` pins that down.

use dam_api::dto::SourceState;
use dam_api::event::{ChangeKind, EventTopic, LibraryEvent, SubscribeRequest};
use dam_api::id::{AssetId, SourceId};
use dam_api::service::{AuthContext, LibraryService};
use dam_client::ApiClient;
use futures::{SinkExt, StreamExt};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio_tungstenite::tungstenite::Message;

// Tungstenite fixes this callback's error to an HTTP response; the test only returns `Ok`, so the
// large, uninhabited-in-practice error path cannot be boxed or replaced at this boundary.
#[allow(clippy::result_large_err)]
async fn accept_ticketed_ws(
    listener: &tokio::net::TcpListener,
    expected_authorization: Option<&str>,
) -> tokio_tungstenite::WebSocketStream<tokio::net::TcpStream> {
    let (mut http, _) = listener.accept().await.unwrap();
    let mut request = Vec::new();
    let mut chunk = [0_u8; 1024];
    while !request.windows(4).any(|window| window == b"\r\n\r\n") {
        let read = http.read(&mut chunk).await.unwrap();
        assert!(read > 0, "ticket request ended before its headers");
        request.extend_from_slice(&chunk[..read]);
    }
    let request = String::from_utf8(request).unwrap();
    assert!(request.starts_with("POST /api/v1/ws-ticket HTTP/1.1\r\n"));
    let authorization = request.lines().find_map(|line| {
        line.split_once(':')
            .filter(|(name, _)| name.eq_ignore_ascii_case("authorization"))
            .map(|(_, value)| value.trim())
    });
    assert_eq!(authorization, expected_authorization);
    let body = r#"{"ticket":"dam_ws_test","expires_in":30}"#;
    http.write_all(
        format!(
            "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
            body.len()
        )
        .as_bytes(),
    )
    .await
    .unwrap();
    drop(http);

    let (stream, _) = listener.accept().await.unwrap();
    tokio_tungstenite::accept_hdr_async(
        stream,
        |request: &tokio_tungstenite::tungstenite::handshake::server::Request,
         response: tokio_tungstenite::tungstenite::handshake::server::Response| {
            assert_eq!(request.uri().query(), Some("ticket=dam_ws_test"));
            assert!(request.headers().get("authorization").is_none());
            Ok(response)
        },
    )
    .await
    .unwrap()
}

/// Spawn a one-shot WS server on an ephemeral port that emits `events` (JSON text frames, like the
/// server's `ws_loop`) to the first client, then holds the socket open briefly. Returns the port.
async fn spawn_ws_server(events: Vec<LibraryEvent>) -> u16 {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        let mut ws = accept_ticketed_ws(&listener, None).await;
        for ev in &events {
            let json = serde_json::to_string(ev).unwrap();
            ws.send(Message::Text(json.into())).await.unwrap();
        }
        // Keep the connection open long enough for the client to drain the frames before close.
        tokio::time::sleep(Duration::from_millis(500)).await;
    });
    port
}

/// Emit a large burst without retaining it in the test process. The client deliberately does not
/// poll its returned stream until this producer has had time to overrun the bounded delivery queue.
async fn spawn_burst_ws_server(count: usize) -> u16 {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        let mut ws = accept_ticketed_ws(&listener, None).await;
        for _ in 0..count {
            let event = LibraryEvent::AssetChanged {
                id: AssetId::new(),
                source_id: None,
                kind: ChangeKind::Metadata,
            };
            let json = serde_json::to_string(&event).unwrap();
            if ws.send(Message::Text(json.into())).await.is_err() {
                break;
            }
        }
    });
    port
}

async fn client_for(port: u16) -> ApiClient {
    let base = format!("http://127.0.0.1:{port}").parse().unwrap();
    ApiClient::connect(base).await.unwrap()
}

#[tokio::test]
async fn bearer_authenticates_ticket_mint_but_not_websocket_upgrade() {
    let id = AssetId::new();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        let mut ws = accept_ticketed_ws(&listener, Some("Bearer native-secret")).await;
        let event = LibraryEvent::AssetChanged {
            id,
            source_id: None,
            kind: ChangeKind::Metadata,
        };
        ws.send(Message::Text(serde_json::to_string(&event).unwrap().into()))
            .await
            .unwrap();
    });
    let base = format!("http://127.0.0.1:{port}").parse().unwrap();
    let client = ApiClient::connect_with_token(base, Some("native-secret".into()))
        .await
        .unwrap();
    let mut stream = client
        .subscribe(&AuthContext::embedded(), SubscribeRequest::default())
        .await
        .unwrap();
    assert!(matches!(
        next_event(&mut stream).await,
        LibraryEvent::AssetChanged { id: received, .. } if received == id
    ));
}

async fn next_event(stream: &mut dam_api::service::EventStream<LibraryEvent>) -> LibraryEvent {
    tokio::time::timeout(Duration::from_secs(5), stream.next())
        .await
        .expect("an event should arrive before the timeout")
        .expect("the stream should yield an event")
}

#[tokio::test]
async fn subscribe_streams_server_events() {
    let id = AssetId::new();
    let port = spawn_ws_server(vec![LibraryEvent::AssetChanged {
        id,
        source_id: None,
        kind: ChangeKind::Retagged,
    }])
    .await;
    let client = client_for(port).await;

    let mut stream = client
        .subscribe(&AuthContext::embedded(), SubscribeRequest::default())
        .await
        .expect("subscribe should connect");

    match next_event(&mut stream).await {
        LibraryEvent::AssetChanged { id: gid, kind, .. } => {
            assert_eq!(gid, id, "the id must round-trip");
            assert_eq!(kind, ChangeKind::Retagged);
        }
        other => panic!("expected AssetChanged, got {other:?}"),
    }
}

/// A removal must survive the wire. It only can because `AssetRemoved` is a struct variant: serde's
/// internally-tagged representation cannot serialize a newtype-with-scalar, so the previous
/// `AssetRemoved(AssetId)` was silently dropped by `ws_loop` and never reached any client. The
/// `source_id` the ceiling check reads must round-trip alongside the id (issue #42).
#[tokio::test]
async fn asset_removed_round_trips() {
    let id = AssetId::new();
    let source_id = SourceId::new();
    let port = spawn_ws_server(vec![LibraryEvent::AssetRemoved {
        id,
        source_id: Some(source_id),
    }])
    .await;
    let client = client_for(port).await;

    let mut stream = client
        .subscribe(&AuthContext::embedded(), SubscribeRequest::default())
        .await
        .expect("subscribe should connect");

    match next_event(&mut stream).await {
        LibraryEvent::AssetRemoved {
            id: gid,
            source_id: gsid,
        } => {
            assert_eq!(gid, id, "the id must round-trip");
            assert_eq!(gsid, Some(source_id), "the attribution must round-trip");
        }
        other => panic!("expected AssetRemoved, got {other:?}"),
    }
}

#[tokio::test]
async fn subscribe_honours_topic_filter() {
    // Server emits a source event then an asset event; a subscription filtered to Assets must skip
    // the source event and deliver only the asset one.
    let id = AssetId::new();
    let port = spawn_ws_server(vec![
        LibraryEvent::SourceState {
            id: SourceId::new(),
            state: SourceState::Online,
        },
        LibraryEvent::AssetChanged {
            id,
            source_id: None,
            kind: ChangeKind::Metadata,
        },
    ])
    .await;
    let client = client_for(port).await;

    let mut stream = client
        .subscribe(
            &AuthContext::embedded(),
            SubscribeRequest {
                topics: vec![EventTopic::Assets],
            },
        )
        .await
        .expect("subscribe should connect");

    // The source event is filtered out, so the first delivered event is the asset one.
    match next_event(&mut stream).await {
        LibraryEvent::AssetChanged { id: gid, .. } => assert_eq!(gid, id),
        other => panic!("expected the source event to be filtered out, got {other:?}"),
    }
}

#[tokio::test]
async fn slow_consumer_gets_bounded_backlog_then_explicit_lag() {
    let port = spawn_burst_ws_server(100_000).await;
    let client = client_for(port).await;
    let mut stream = client
        .subscribe(&AuthContext::embedded(), SubscribeRequest::default())
        .await
        .expect("subscribe should connect");

    // Let the socket pump run while the returned stream is intentionally unpolled.
    tokio::time::sleep(Duration::from_millis(100)).await;

    let delivered_before_lag = tokio::time::timeout(Duration::from_secs(5), async {
        let mut delivered = 0;
        loop {
            match stream
                .next()
                .await
                .expect("subscription should remain open")
            {
                LibraryEvent::StreamLagged => break delivered,
                _ => delivered += 1,
            }
        }
    })
    .await
    .expect("bounded delivery must eventually announce lag");

    assert!(
        delivered_before_lag <= 258,
        "100k inputs produced an unexpectedly large retained backlog: {delivered_before_lag}"
    );
}
