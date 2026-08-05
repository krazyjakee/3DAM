//! The `/api/v1/ws` live-event socket: upgrade under a one-use ticket, then pump engine events to
//! the client until shutdown, credential revocation, or disconnect.

use crate::tickets::ticket_key;
use crate::{auth, ApiError, AppState};
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Query, State};
use axum::response::{IntoResponse, Response};
use dam_api::event::SubscribeRequest;
use dam_api::service::LibraryService;
use dam_api::LibError;
use futures::StreamExt;
use std::time::{Duration, Instant};

/// Auth for the WebSocket firehose: the URI carries only a short-lived one-use ticket minted by an
/// authenticated header/cookie request. A long-lived bearer is never accepted here (issue #128).
#[derive(serde::Deserialize)]
pub(crate) struct WsAuthQuery {
    ticket: String,
}

pub(crate) async fn ws_handler(
    ws: WebSocketUpgrade,
    Query(q): Query<WsAuthQuery>,
    State(st): State<AppState>,
) -> Response {
    let ticket = match st.ws_tickets.lock() {
        Ok(mut tickets) => tickets.remove(&ticket_key(&q.ticket)),
        Err(_) => {
            return ApiError(LibError::Internal("ticket service unavailable".into()))
                .into_response()
        }
    };
    let Some(ticket) = ticket.filter(|ticket| ticket.expires > Instant::now()) else {
        return ApiError(LibError::Unauthorized).into_response();
    };
    let ctx = if ticket.token.is_some() || ticket.cookie.is_some() {
        match auth::resolve_request(&st, ticket.token.clone(), ticket.cookie.clone()).await {
            Ok(resolved) if resolved.ctx.scopes.has(dam_api::service::Scope::Read) => resolved.ctx,
            _ => return ApiError(LibError::Unauthorized).into_response(),
        }
    } else {
        ticket.ctx
    };
    ws.on_upgrade(move |socket| ws_loop(socket, st, ctx, ticket.token, ticket.cookie))
}

async fn ws_loop(
    mut socket: WebSocket,
    st: AppState,
    ctx: dam_api::service::AuthContext,
    token: Option<String>,
    cookie: Option<String>,
) {
    // Subscribe under the *caller's* context, not the embedded owner — the engine filters the
    // stream to the ceiling (restricted subscribers get no job/asset payloads; issue #42).
    let mut vis_gen = st.store.visibility_generation();
    let mut stream = match st.lib.subscribe(&ctx, SubscribeRequest::default()).await {
        Ok(s) => s,
        Err(_) => return,
    };
    let mut shutdown = st.shutdown.clone();
    let mut auth_check = tokio::time::interval(Duration::from_secs(30));
    loop {
        tokio::select! {
            // Server is stopping: send a courteous Close frame and let the drain complete.
            _ = shutdown.changed() => {
                let _ = socket.send(Message::Close(None)).await;
                break;
            }
            _ = auth_check.tick(), if token.is_some() || cookie.is_some() => {
                // Revoking the parent token/session closes an already-open socket within one
                // ticket TTL, independently of catalog traffic or visibility mutations.
                match auth::resolve_request(&st, token.clone(), cookie.clone()).await {
                    Ok(resolved) if resolved.ctx.scopes.has(dam_api::service::Scope::Read) => {}
                    _ => {
                        let _ = socket.send(Message::Close(None)).await;
                        break;
                    }
                }
            }
            ev = stream.next() => {
                let Some(ev) = ev else { break }; // event bus closed (engine shutting down)
                // A share/group/account mutation since we subscribed: re-resolve the credential
                // and re-subscribe under the fresh ceiling; a now-invalid credential closes.
                let gen_now = st.store.visibility_generation();
                if gen_now != vis_gen {
                    vis_gen = gen_now;
                    match auth::resolve_request(&st, token.clone(), cookie.clone()).await {
                        Ok(r) if r.ctx.scopes.has(dam_api::service::Scope::Read) => {
                            match st.lib.subscribe(&r.ctx, SubscribeRequest::default()).await {
                                Ok(s) => stream = s,
                                Err(_) => break,
                            }
                        }
                        _ => {
                            let _ = socket.send(Message::Close(None)).await;
                            break;
                        }
                    }
                    continue; // the in-flight event predates the fresh ceiling — drop it
                }
                match serde_json::to_string(&ev) {
                    Ok(txt) => {
                        if socket.send(Message::Text(txt.into())).await.is_err() {
                            break;
                        }
                    }
                    Err(_) => continue,
                }
            }
        }
    }
}
