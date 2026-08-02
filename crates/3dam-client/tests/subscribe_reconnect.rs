//! Characterizes the v1 reconnect contract: the client reconnects the same subscription, emits one
//! explicit lag marker for the unresumable gap, and preserves the requested topic filter.

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
    let body = r#"{"ticket":"dam_ws_reconnect","expires_in":30}"#;
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
            assert_eq!(request.uri().query(), Some("ticket=dam_ws_reconnect"));
            assert!(request.headers().get("authorization").is_none());
            Ok(response)
        },
    )
    .await
    .unwrap()
}

async fn spawn_reconnecting_server(asset_ids: [AssetId; 2]) -> u16 {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        for asset_id in asset_ids {
            let mut ws = accept_ticketed_ws(&listener).await;
            let events = [
                LibraryEvent::SourceState {
                    id: SourceId::new(),
                    state: SourceState::Online,
                },
                LibraryEvent::AssetChanged {
                    id: asset_id,
                    source_id: None,
                    kind: ChangeKind::Metadata,
                },
            ];
            for event in events {
                ws.send(Message::Text(serde_json::to_string(&event).unwrap().into()))
                    .await
                    .unwrap();
            }
            ws.close(None).await.unwrap();
        }
    });
    port
}

async fn next_asset(stream: &mut dam_api::service::EventStream<LibraryEvent>) -> AssetId {
    match tokio::time::timeout(Duration::from_secs(5), stream.next())
        .await
        .expect("event timed out")
        .expect("stream ended before its asset event")
    {
        LibraryEvent::AssetChanged { id, .. } => id,
        other => panic!("topic filter should suppress the source event, got {other:?}"),
    }
}

#[tokio::test]
async fn reconnect_emits_one_lag_marker_and_reuses_the_topic_filter() {
    let expected = [AssetId::new(), AssetId::new()];
    let port = spawn_reconnecting_server(expected).await;
    let client = ApiClient::connect(format!("http://127.0.0.1:{port}").parse().unwrap())
        .await
        .unwrap();
    let mut stream = client
        .subscribe(
            &AuthContext::embedded(),
            SubscribeRequest {
                topics: vec![EventTopic::Assets],
            },
        )
        .await
        .unwrap();
    assert_eq!(next_asset(&mut stream).await, expected[0]);
    assert!(matches!(
        tokio::time::timeout(Duration::from_secs(5), stream.next())
            .await
            .expect("reconnect lag marker timed out")
            .expect("subscription should survive reconnect"),
        LibraryEvent::StreamLagged
    ));
    assert_eq!(next_asset(&mut stream).await, expected[1]);
}
