use anyhow::Result;
use axum::{
    extract::{Query, State},
    http::{HeaderMap, StatusCode},
    response::sse::{Event, Sse},
    routing::get,
    Json, Router,
};
use futures::{stream::Stream, SinkExt, StreamExt};
use rmcp::{
    model::*, serve_server, service::RunningService,
    ClientHandlerService, ServerHandlerService,
};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::io::{self};
use tokio::sync::Mutex;

use crate::relay::Relay;
use crate::rbac;
type SessionId = Arc<str>;

fn session_id() -> SessionId {
    let id = format!("{:016x}", rand::random::<u128>());
    Arc::from(id)
}

#[derive(Clone, Default)]
pub struct App {
    rules: Vec<rbac::Rule>,
    services: HashMap<String, Arc<Mutex<RunningService<ClientHandlerService>>>>,
    txs: Arc<
        tokio::sync::RwLock<HashMap<SessionId, tokio::sync::mpsc::Sender<ClientJsonRpcMessage>>>,
    >,
}

impl App {
    pub fn new(services: HashMap<String, Arc<Mutex<RunningService<ClientHandlerService>>>>) -> Self {
        Self {
            rules: vec![],
            txs: Default::default(),
            services: services,
        }
    }
    pub fn router(&self) -> Router {
        Router::new()
            .route("/sse", get(sse_handler).post(post_event_handler))
            .with_state(self.clone())
    }
}

#[derive(Debug, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PostEventQuery {
    pub session_id: String,
}

async fn post_event_handler(
    State(app): State<App>,
    Query(PostEventQuery { session_id }): Query<PostEventQuery>,
    Json(message): Json<ClientJsonRpcMessage>,
) -> Result<StatusCode, StatusCode> {
    tracing::info!(session_id, ?message, "new client message");
    let tx = {
        let rg = app.txs.read().await;
        rg.get(session_id.as_str())
            .ok_or(StatusCode::NOT_FOUND)?
            .clone()
    };
    if tx.send(message).await.is_err() {
        tracing::error!("send message error");
        return Err(StatusCode::GONE);
    }
    Ok(StatusCode::ACCEPTED)
}

async fn sse_handler(
    State(app): State<App>,
    headers: HeaderMap,
) -> Sse<impl Stream<Item = Result<Event, io::Error>>> {
    // it's 4KB

    let claims = rbac::Claims::new(&headers);
    let rbac = rbac::RbacEngine::new(app.rules, claims);
    let session = session_id();
    tracing::info!(%session, "sse connection");
    use tokio_stream::wrappers::ReceiverStream;
    use tokio_util::sync::PollSender;
    let (from_client_tx, from_client_rx) = tokio::sync::mpsc::channel(64);
    let (to_client_tx, to_client_rx) = tokio::sync::mpsc::channel(64);
    app.txs
        .write()
        .await
        .insert(session.clone(), from_client_tx);
    {
        let session = session.clone();
        tokio::spawn(async move {
            let service = ServerHandlerService::new(Relay {
                services: app.services,
                rbac: rbac,
            });
            let stream = ReceiverStream::new(from_client_rx);
            let sink = PollSender::new(to_client_tx).sink_map_err(std::io::Error::other);
            let result = serve_server(service, (sink, stream))
                .await
                .inspect_err(|e| {
                    tracing::error!("serving error: {:?}", e);
                });

            if let Err(e) = result {
                tracing::error!(error = ?e, "initialize error");
                app.txs.write().await.remove(&session);
                return;
            }
            let _running_result = result.unwrap().waiting().await.inspect_err(|e| {
                tracing::error!(error = ?e, "running error");
            });
            app.txs.write().await.remove(&session);
        });
    }

    let stream = futures::stream::once(futures::future::ok(
        Event::default()
            .event("endpoint")
            .data(format!("?sessionId={session}")),
    ))
    .chain(ReceiverStream::new(to_client_rx).map(|message| {
        match serde_json::to_string(&message) {
            Ok(bytes) => Ok(Event::default().event("message").data(&bytes)),
            Err(e) => Err(io::Error::new(io::ErrorKind::InvalidData, e)),
        }
    }));
    Sse::new(stream)
}