use std::{collections::BTreeMap, path::Path, sync::Arc};

use axum::{
    Json, Router,
    body::Bytes,
    extract::{DefaultBodyLimit, Path as RoutePath, Query, Request, State},
    http::{StatusCode, header},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use kelp_core::{
    ApiError, MAX_BLOB_BYTES, MAX_METADATA_BYTES, object_id,
    transactions::{JournalPage, PROTOCOL, Project, Receipt, SyncPage, SyncRequest, Transaction},
    transfer::{self, Info},
    validate_hash, validate_name,
};
use serde::Deserialize;

use crate::{Error, backend::Backend};

#[derive(Clone)]
struct GraphState {
    backend: Backend,
    token: Arc<str>,
}

pub fn local_app(data: &Path, token: String) -> anyhow::Result<Router> {
    routes(Backend::local(data)?, token, false)
}

pub fn cluster_app(
    nodes: Vec<String>,
    token: String,
    storage_token: String,
) -> anyhow::Result<Router> {
    Ok(
        routes(Backend::cluster(nodes, storage_token)?, token, false)?
            .route("/healthz", get(|| async { "ok\n" })),
    )
}

pub fn storage_app(data: &Path, token: String) -> anyhow::Result<Router> {
    Ok(routes(Backend::local(data)?, token, true)?.route("/healthz", get(|| async { "ok\n" })))
}

fn routes(backend: Backend, token: String, storage: bool) -> anyhow::Result<Router> {
    anyhow::ensure!(!token.trim().is_empty(), "access token must not be empty");
    let state = GraphState {
        backend,
        token: token.into(),
    };
    let router = if storage {
        Router::new()
            .route(
                "/storage/projects/{project}",
                get(project).put(create_project),
            )
            .route(
                "/storage/projects/{project}/objects/{kind}/{id}",
                get(object).head(object_size).put(upload),
            )
            .route(
                "/storage/projects/{project}/transactions",
                post(append_verified),
            )
            .route("/storage/projects/{project}/journal", get(journal))
            .route(
                "/storage/projects/{project}/objects/info",
                post(object_info),
            )
            .route(
                "/storage/projects/{project}/objects/download",
                post(download_batch),
            )
            .route(
                "/storage/projects/{project}/objects/upload",
                post(upload_batch),
            )
    } else {
        Router::new()
            .route("/v1/projects/{project}", get(project).put(create_project))
            .route(
                "/v1/projects/{project}/objects/{kind}/{id}",
                get(object).head(object_size).put(upload),
            )
            .route("/v1/projects/{project}/transactions", post(push))
            .route("/v1/projects/{project}/sync", post(sync))
            .route("/v1/projects/{project}/objects/info", post(object_info))
            .route(
                "/v1/projects/{project}/objects/download",
                post(download_batch),
            )
            .route("/v1/projects/{project}/objects/upload", post(upload_batch))
    };
    Ok(router
        .layer(DefaultBodyLimit::max(
            transfer::MAX_PACK_BYTES + 1024 * 1024,
        ))
        .route_layer(middleware::from_fn_with_state(state.clone(), authenticate))
        .with_state(state))
}

async fn authenticate(State(state): State<GraphState>, request: Request, next: Next) -> Response {
    let token = request
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "));
    if token != Some(state.token.as_ref()) {
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

fn invalid(error: impl std::fmt::Display) -> Error {
    Error::Invalid(error.to_string())
}

fn info(state: &GraphState, project: String) -> Project {
    Project {
        project,
        protocol: PROTOCOL.into(),
        layout: state.backend.layout(),
        storage_nodes: state.backend.nodes(),
    }
}

async fn create_project(
    State(state): State<GraphState>,
    RoutePath(project): RoutePath<String>,
) -> Result<Json<Project>, Error> {
    validate_name(&project).map_err(invalid)?;
    state.backend.project(&project, true).await?;
    Ok(Json(info(&state, project)))
}

async fn project(
    State(state): State<GraphState>,
    RoutePath(project): RoutePath<String>,
) -> Result<Json<Project>, Error> {
    validate_name(&project).map_err(invalid)?;
    state.backend.project(&project, false).await?;
    Ok(Json(info(&state, project)))
}

fn validate_object(project: &str, kind: &str, id: &str) -> Result<(), Error> {
    validate_name(project).map_err(invalid)?;
    validate_hash(id).map_err(invalid)?;
    if !matches!(kind, "blob" | "transaction") {
        return Err(invalid("unsupported object kind"));
    }
    Ok(())
}

async fn object(
    State(state): State<GraphState>,
    RoutePath((project, kind, id)): RoutePath<(String, String, String)>,
) -> Result<impl IntoResponse, Error> {
    validate_object(&project, &kind, &id)?;
    let bytes = state.backend.get(&project, &kind, &id).await?;
    Ok(([(header::CONTENT_TYPE, "application/octet-stream")], bytes))
}

async fn object_size(
    State(state): State<GraphState>,
    RoutePath((project, kind, id)): RoutePath<(String, String, String)>,
) -> Result<impl IntoResponse, Error> {
    validate_object(&project, &kind, &id)?;
    let size = state.backend.size(&project, &kind, &id).await?;
    Ok(([(header::CONTENT_LENGTH, size.to_string())], ()))
}

async fn upload(
    State(state): State<GraphState>,
    RoutePath((project, kind, id)): RoutePath<(String, String, String)>,
    bytes: Bytes,
) -> Result<StatusCode, Error> {
    validate_object(&project, &kind, &id)?;
    if kind != "blob" || object_id("blob", &bytes) != id {
        return Err(invalid(
            "only hash-verified file objects can be uploaded directly",
        ));
    }
    if bytes.len() > MAX_BLOB_BYTES {
        return Err(invalid("file object exceeds size limit"));
    }
    state
        .backend
        .put_blob(&project, &id, bytes.to_vec())
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

fn transaction(bytes: &[u8]) -> Result<Transaction, Error> {
    if bytes.len() > MAX_METADATA_BYTES {
        return Err(invalid("transaction exceeds 2 MiB"));
    }
    let transaction: Transaction = serde_json::from_slice(bytes).map_err(invalid)?;
    transaction.validate().map_err(invalid)?;
    Ok(transaction)
}

async fn push(
    State(state): State<GraphState>,
    RoutePath(project): RoutePath<String>,
    bytes: Bytes,
) -> Result<Json<Receipt>, Error> {
    validate_name(&project).map_err(invalid)?;
    let transaction = transaction(&bytes)?;
    let id = transaction.id()?;
    match state.backend.size(&project, "transaction", &id).await {
        Ok(_) => return Ok(Json(Receipt { transaction: id })),
        Err(Error::Missing(_)) => {}
        Err(error) => return Err(error),
    }
    let mut parents = BTreeMap::new();
    let parent_keys: Vec<_> = transaction
        .dependencies()
        .into_iter()
        .map(|id| transfer::Key {
            kind: "transaction".into(),
            id,
        })
        .collect();
    for chunk in parent_keys.chunks(transfer::MAX_OBJECTS) {
        for object in state.backend.get_batch(&project, chunk.to_vec()).await? {
            parents.insert(
                object.key.id,
                serde_json::from_slice(&object.bytes).map_err(invalid)?,
            );
        }
    }
    transaction.validate_parents(&parents).map_err(invalid)?;
    let blob_keys: std::collections::BTreeSet<_> = transaction
        .edits
        .values()
        .filter_map(|edit| edit.value.as_ref())
        .map(|value| transfer::Key {
            kind: "blob".into(),
            id: value.blob.clone(),
        })
        .collect();
    let blob_keys: Vec<_> = blob_keys.into_iter().collect();
    let mut sizes = BTreeMap::new();
    for chunk in blob_keys.chunks(transfer::MAX_OBJECTS) {
        for info in state.backend.info_batch(&project, chunk.to_vec()).await? {
            sizes.insert(info.object.id, info.size);
        }
    }
    for (path, edit) in &transaction.edits {
        if let Some(value) = &edit.value
            && sizes.get(&value.blob) != Some(&Some(value.size))
        {
            return Err(invalid(format!("file size mismatch for {path}")));
        }
    }
    Ok(Json(Receipt {
        transaction: state.backend.append(&project, transaction).await?,
    }))
}

async fn object_info(
    State(state): State<GraphState>,
    RoutePath(project): RoutePath<String>,
    Json(request): Json<transfer::Request>,
) -> Result<Json<Vec<Info>>, Error> {
    validate_name(&project).map_err(invalid)?;
    request.validate().map_err(invalid)?;
    Ok(Json(
        state.backend.info_batch(&project, request.objects).await?,
    ))
}

async fn download_batch(
    State(state): State<GraphState>,
    RoutePath(project): RoutePath<String>,
    Json(request): Json<transfer::Request>,
) -> Result<impl IntoResponse, Error> {
    validate_name(&project).map_err(invalid)?;
    request.validate().map_err(invalid)?;
    let sizes = state
        .backend
        .info_batch(&project, request.objects.clone())
        .await?;
    let mut total = 4_u64;
    for info in sizes {
        let size = info
            .size
            .ok_or_else(|| Error::Missing(format!("missing {}", info.object.id)))?;
        total += size + 69;
    }
    if total > transfer::MAX_PACK_BYTES as u64 {
        return Err(invalid("requested object batch exceeds byte limit"));
    }
    let objects = state.backend.get_batch(&project, request.objects).await?;
    let bytes = tokio::task::spawn_blocking(move || transfer::encode(&objects))
        .await
        .map_err(|error| Error::Internal(error.into()))??;
    Ok(([(header::CONTENT_TYPE, "application/x-kelp-pack")], bytes))
}

async fn upload_batch(
    State(state): State<GraphState>,
    RoutePath(project): RoutePath<String>,
    bytes: Bytes,
) -> Result<StatusCode, Error> {
    validate_name(&project).map_err(invalid)?;
    let objects = tokio::task::spawn_blocking(move || transfer::decode(&bytes))
        .await
        .map_err(|error| Error::Internal(error.into()))?
        .map_err(invalid)?;
    if objects.iter().any(|object| object.key.kind != "blob") {
        return Err(invalid(
            "transaction publication uses the transactions endpoint",
        ));
    }
    state.backend.put_batch(&project, objects).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Only trusted gateways reach the storage API, using a separate storage token.
async fn append_verified(
    State(state): State<GraphState>,
    RoutePath(project): RoutePath<String>,
    bytes: Bytes,
) -> Result<Json<Receipt>, Error> {
    validate_name(&project).map_err(invalid)?;
    let transaction = transaction(&bytes)?;
    Ok(Json(Receipt {
        transaction: state.backend.append(&project, transaction).await?,
    }))
}

#[derive(Deserialize)]
struct JournalQuery {
    #[serde(default)]
    after: i64,
}

async fn journal(
    State(state): State<GraphState>,
    RoutePath(project): RoutePath<String>,
    Query(query): Query<JournalQuery>,
) -> Result<Json<JournalPage>, Error> {
    validate_name(&project).map_err(invalid)?;
    Ok(Json(state.backend.journal(&project, query.after).await?))
}

async fn sync(
    State(state): State<GraphState>,
    RoutePath(project): RoutePath<String>,
    Json(request): Json<SyncRequest>,
) -> Result<Json<SyncPage>, Error> {
    validate_name(&project).map_err(invalid)?;
    Ok(Json(state.backend.sync(&project, request.cursors).await?))
}
