//! Integration test for `ApiClient::subscribe` (issue #36): an event emitted "server-side" over the
//! WebSocket firehose must reach an `ApiClient` subscriber as a `LibraryEvent`. A minimal in-test WS
//! server stands in for `3dam serve`'s `/api/v1/ws` — it does exactly what `ws_loop` does (send each
//! event JSON-serialized as a text frame), so this exercises the real client transport end to end.
//!
//! NB: the events used here are the *struct*-shaped variants. `LibraryEvent::AssetRemoved(AssetId)`
//! and other newtype-with-scalar variants can't be serialized under the enum's internally-tagged
//! `#[serde(tag = "type")]` representation — a separate latent bug (they're silently dropped by the
//! server's `ws_loop`), tracked outside this issue.

use dam_api::dto::SourceState;
use dam_api::event::{ChangeKind, EventTopic, LibraryEvent, SubscribeRequest};
use dam_api::id::{AssetId, SourceId};
use dam_api::service::{AuthContext, LibraryService};
use dam_client::ApiClient;
use futures::{SinkExt, StreamExt};
use std::time::Duration;
use tokio_tungstenite::tungstenite::Message;

/// Spawn a one-shot WS server on an ephemeral port that emits `events` (JSON text frames, like the
/// server's `ws_loop`) to the first client, then holds the socket open briefly. Returns the port.
async fn spawn_ws_server(events: Vec<LibraryEvent>) -> u16 {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut ws = tokio_tungstenite::accept_async(stream).await.unwrap();
        for ev in &events {
            let json = serde_json::to_string(ev).unwrap();
            ws.send(Message::Text(json.into())).await.unwrap();
        }
        // Keep the connection open long enough for the client to drain the frames before close.
        tokio::time::sleep(Duration::from_millis(500)).await;
    });
    port
}

async fn client_for(port: u16) -> ApiClient {
    let base = format!("http://127.0.0.1:{port}").parse().unwrap();
    ApiClient::connect(base).await.unwrap()
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
        kind: ChangeKind::Retagged,
    }])
    .await;
    let client = client_for(port).await;

    let mut stream = client
        .subscribe(&AuthContext::embedded(), SubscribeRequest::default())
        .await
        .expect("subscribe should connect");

    match next_event(&mut stream).await {
        LibraryEvent::AssetChanged { id: gid, kind } => {
            assert_eq!(gid, id, "the id must round-trip");
            assert_eq!(kind, ChangeKind::Retagged);
        }
        other => panic!("expected AssetChanged, got {other:?}"),
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
