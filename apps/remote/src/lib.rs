//! Transaction gateways, storage nodes, and the standalone compatibility API.

mod backend;
mod database;
mod graph_api;

pub use graph_api::{cluster_app, storage_app};

use std::{
    path::Path,
    sync::{Arc, Mutex},
};

use axum::{
    Json, Router,
    body::Bytes,
    extract::{DefaultBodyLimit, Path as RoutePath, Request, State},
    http::{StatusCode, header},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use kelp_core::{
    ApiError, ChangeInfo, MAX_BLOB_BYTES, ProjectInfo, Publication, PublicationReceipt,
};
use rusqlite::Connection;

#[derive(Clone)]
struct AppState {
    db: Arc<Mutex<Connection>>,
    token: Arc<str>,
}

impl AppState {
    async fn run<T: Send + 'static>(
        &self,
        operation: impl FnOnce(&mut Connection) -> Result<T, Error> + Send + 'static,
    ) -> Result<T, Error> {
        let db = self.db.clone();
        tokio::task::spawn_blocking(move || {
            let mut db = db
                .lock()
                .map_err(|_| Error::Internal(anyhow::anyhow!("database lock poisoned")))?;
            operation(&mut db)
        })
        .await
        .map_err(|e| Error::Internal(e.into()))?
    }
}

#[derive(Debug, thiserror::Error)]
enum Error {
    #[error("{0}")]
    Invalid(String),
    #[error("{0}")]
    Missing(String),
    #[error("{0}")]
    Conflict(String),
    #[error("{0}")]
    Unavailable(String),
    #[error(transparent)]
    Internal(#[from] anyhow::Error),
}

impl IntoResponse for Error {
    fn into_response(self) -> Response {
        let (status, code) = match &self {
            Self::Invalid(_) => (StatusCode::BAD_REQUEST, "INVALID_REQUEST"),
            Self::Missing(_) => (StatusCode::NOT_FOUND, "NOT_FOUND"),
            Self::Conflict(_) => (StatusCode::CONFLICT, "CONFLICT"),
            Self::Unavailable(_) => (StatusCode::SERVICE_UNAVAILABLE, "UNAVAILABLE"),
            Self::Internal(error) => {
                tracing::error!(error = ?error, "request failed");
                (StatusCode::INTERNAL_SERVER_ERROR, "INTERNAL_ERROR")
            }
        };
        let message = if status.is_server_error() {
            "internal storage error".into()
        } else {
            self.to_string()
        };
        (
            status,
            Json(ApiError {
                code: code.into(),
                message,
            }),
        )
            .into_response()
    }
}

/// Build the service using a persistent database and one shared access token.
pub fn app(data_dir: &Path, token: String) -> anyhow::Result<Router> {
    anyhow::ensure!(!token.trim().is_empty(), "KELP_TOKEN must not be empty");
    std::fs::create_dir_all(data_dir)?;
    let db = database::open(&data_dir.join("kelp.sqlite3"))?;
    let transactions = graph_api::local_app(data_dir, token.clone())?;
    let state = AppState {
        db: Arc::new(Mutex::new(db)),
        token: token.into(),
    };
    let api = Router::new()
        .route("/v0/projects/{project}", get(project).put(create_project))
        .route(
            "/v0/projects/{project}/objects/{kind}/{id}",
            get(object).put(upload),
        )
        .route("/v0/projects/{project}/changes", get(changes))
        .route("/v0/projects/{project}/changes/{change}", get(change))
        .route(
            "/v0/projects/{project}/changes/{change}/publications",
            post(publish),
        )
        .layer(DefaultBodyLimit::max(MAX_BLOB_BYTES))
        .route_layer(middleware::from_fn_with_state(state.clone(), authenticate));
    Ok(Router::new()
        .route("/healthz", get(|| async { "ok\n" }))
        .merge(api)
        .with_state(state)
        .merge(transactions))
}

async fn authenticate(State(state): State<AppState>, request: Request, next: Next) -> Response {
    let provided = request
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "));
    if provided != Some(state.token.as_ref()) {
        return (
            StatusCode::UNAUTHORIZED,
            Json(ApiError {
                code: "UNAUTHORIZED".into(),
                message: "a valid bearer token is required".into(),
            }),
        )
            .into_response();
    }
    next.run(request).await
}

async fn create_project(
    State(state): State<AppState>,
    RoutePath(project): RoutePath<String>,
) -> Result<Json<ProjectInfo>, Error> {
    state
        .run(move |db| database::create_project(db, &project))
        .await
        .map(Json)
}

async fn project(
    State(state): State<AppState>,
    RoutePath(project): RoutePath<String>,
) -> Result<Json<ProjectInfo>, Error> {
    state
        .run(move |db| database::project(db, &project))
        .await
        .map(Json)
}

async fn upload(
    State(state): State<AppState>,
    RoutePath((project, kind, id)): RoutePath<(String, String, String)>,
    bytes: Bytes,
) -> Result<StatusCode, Error> {
    state
        .run(move |db| database::upload(db, &project, &kind, &id, &bytes))
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn object(
    State(state): State<AppState>,
    RoutePath((project, kind, id)): RoutePath<(String, String, String)>,
) -> Result<impl IntoResponse, Error> {
    let bytes = state
        .run(move |db| database::object(db, &project, &kind, &id))
        .await?;
    Ok(([(header::CONTENT_TYPE, "application/octet-stream")], bytes))
}

async fn changes(
    State(state): State<AppState>,
    RoutePath(project): RoutePath<String>,
) -> Result<Json<Vec<ChangeInfo>>, Error> {
    state
        .run(move |db| database::changes(db, &project))
        .await
        .map(Json)
}

async fn change(
    State(state): State<AppState>,
    RoutePath((project, change)): RoutePath<(String, String)>,
) -> Result<Json<ChangeInfo>, Error> {
    state
        .run(move |db| database::change(db, &project, &change))
        .await
        .map(Json)
}

async fn publish(
    State(state): State<AppState>,
    RoutePath((project, change)): RoutePath<(String, String)>,
    body: Bytes,
) -> Result<Json<PublicationReceipt>, Error> {
    if body.len() > kelp_core::MAX_METADATA_BYTES {
        return Err(Error::Invalid(
            "publication exceeds metadata size limit".into(),
        ));
    }
    let publication: Publication =
        serde_json::from_slice(&body).map_err(|e| Error::Invalid(e.to_string()))?;
    state
        .run(move |db| database::publish(db, &project, &change, publication))
        .await
        .map(Json)
}
