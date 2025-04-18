use crate::authn;
use crate::relay;
use crate::relay::Relay;
use crate::xds::XdsStore as AppState;
use crate::{proxyprotocol, rbac};
use anyhow::Result;
use axum::extract::{ConnectInfo, OptionalFromRequestParts};
use axum::{
	Json, RequestPartsExt, Router,
	extract::{Query, State},
	http::{StatusCode, request::Parts},
	response::sse::{Event, Sse},
	response::{IntoResponse, Response},
	routing::get,
};
use axum_extra::typed_header::TypedHeaderRejection;
use axum_extra::{
	TypedHeader,
	headers::{Authorization, authorization::Bearer},
};
use futures::{SinkExt, StreamExt, stream::Stream};
use rmcp::model::ClientJsonRpcMessage;
use rmcp::serve_server;
use serde_json::json;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::io::{self};
use tokio::sync::RwLock;
type SessionId = Arc<str>;

fn session_id() -> SessionId {
	let id = format!("{:016x}", rand::random::<u128>());
	Arc::from(id)
}

#[derive(Clone)]
pub struct App {
	state: Arc<std::sync::RwLock<AppState>>,
	txs:
		Arc<tokio::sync::RwLock<HashMap<SessionId, tokio::sync::mpsc::Sender<ClientJsonRpcMessage>>>>,
	metrics: Arc<relay::metrics::Metrics>,
	authn: Arc<RwLock<Option<authn::JwtAuthenticator>>>,
}

impl App {
	pub fn new(
		state: Arc<std::sync::RwLock<AppState>>,
		metrics: Arc<relay::metrics::Metrics>,
		authn: Arc<RwLock<Option<authn::JwtAuthenticator>>>,
	) -> Self {
		Self {
			state,
			txs: Default::default(),
			metrics,
			authn,
		}
	}
	pub fn router(&self) -> Router {
		Router::new()
			.route("/sse", get(sse_handler).post(post_event_handler))
			.with_state(self.clone())
	}
}

impl OptionalFromRequestParts<App> for rbac::Claims {
	type Rejection = AuthError;

	async fn from_request_parts(
		parts: &mut Parts,
		state: &App,
	) -> Result<Option<Self>, Self::Rejection> {
		let authn = state.authn.read().await;
		match authn.as_ref() {
			Some(authn) => {
				tracing::info!("jwt");
				let TypedHeader(Authorization(bearer)) = parts
					.extract::<TypedHeader<Authorization<Bearer>>>()
					.await
					.map_err(AuthError::NoAuthHeaderPresent)?;
				tracing::info!("bearer: {}", bearer.token());
				let claims = authn.authenticate(bearer.token()).await;
				match claims {
					Ok(claims) => Ok(Some(claims)),
					Err(e) => Err(AuthError::JwtError(e)),
				}
			},
			None => Ok(None),
		}
	}
}

impl IntoResponse for AuthError {
	fn into_response(self) -> Response {
		let (status, error_message) = match self {
			AuthError::NoAuthHeaderPresent(e) => (
				StatusCode::BAD_REQUEST,
				format!("No auth header present, error: {}", e),
			),
			AuthError::JwtError(e) => (
				StatusCode::BAD_REQUEST,
				match e {
					authn::AuthError::InvalidToken(e) => format!("Invalid token, error: {}", e),
				},
			),
		};
		let body = Json(json!({
				"error": error_message,
		}));
		(status, body).into_response()
	}
}

#[derive(Debug)]
pub enum AuthError {
	NoAuthHeaderPresent(TypedHeaderRejection),
	JwtError(authn::AuthError),
}

#[derive(Debug, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PostEventQuery {
	pub session_id: String,
}

async fn post_event_handler(
	State(app): State<App>,
	ConnectInfo(_connection): ConnectInfo<proxyprotocol::Address>,
	_claims: Option<rbac::Claims>,
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

//get <-
//-> post

async fn sse_handler(
	State(app): State<App>,
	ConnectInfo(connection): ConnectInfo<proxyprotocol::Address>,
	claims: Option<rbac::Claims>,
) -> Sse<impl Stream<Item = Result<Event, io::Error>>> {
	// it's 4KB

	let session = session_id();
	tracing::info!(%session, ?connection, "sse connection");
	let claims = rbac::Identity::new(
		claims.map(|c| c.0),
		connection.identity.map(|i| i.to_string()),
	);
	use tokio_stream::wrappers::ReceiverStream;
	use tokio_util::sync::PollSender;
	let (from_client_tx, from_client_rx) = tokio::sync::mpsc::channel(64);
	let (to_client_tx, to_client_rx) = tokio::sync::mpsc::channel(64);
	app
		.txs
		.write()
		.await
		.insert(session.clone(), from_client_tx);
	{
		let session = session.clone();
		tokio::spawn(async move {
			let stream = ReceiverStream::new(from_client_rx);
			let sink = PollSender::new(to_client_tx).sink_map_err(std::io::Error::other);
			let result = serve_server(
				Relay::new(app.state.clone(), claims, app.metrics.clone()),
				(sink, stream),
			)
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
