use axum::{
    Router,
    extract::{Path, State, Json},
    http::StatusCode,
    response::IntoResponse,
    routing::{get, post, delete},
};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;

use crate::engine::{EngineError, NamespaceManager};
use crate::types::*;

pub type AppState = Arc<NamespaceManager>;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/health", get(health))
        .route("/v1/namespaces", post(create_namespace))
        .route("/v1/namespaces", get(list_namespaces))
        .route("/v1/namespaces/{ns}", delete(delete_namespace))
        .route("/v1/namespaces/{ns}/documents", post(upsert_documents))
        .route("/v1/namespaces/{ns}/documents", delete(delete_documents))
        .route("/v1/namespaces/{ns}/documents/{id}", get(get_document))
        .route("/v1/namespaces/{ns}/query", post(query))
}

async fn health() -> &'static str {
    "ok"
}

#[derive(Deserialize)]
struct CreateNamespaceReq {
    name: String,
    #[serde(default)]
    distance_metric: DistanceMetric,
}

#[derive(Serialize)]
struct NamespaceInfo {
    name: String,
    dimensions: Option<usize>,
    distance_metric: DistanceMetric,
    doc_count: u64,
}

async fn create_namespace(
    State(mgr): State<AppState>,
    Json(req): Json<CreateNamespaceReq>,
) -> Result<impl IntoResponse, ApiError> {
    mgr.create_namespace(req.name, req.distance_metric).await?;
    Ok(StatusCode::CREATED)
}

async fn list_namespaces(
    State(mgr): State<AppState>,
) -> Json<Vec<NamespaceInfo>> {
    let ns = mgr.list_namespaces().await;
    let infos: Vec<NamespaceInfo> = ns.into_iter().map(|m| NamespaceInfo {
        name: m.name,
        dimensions: m.dimensions,
        distance_metric: m.distance_metric,
        doc_count: m.doc_count,
    }).collect();
    Json(infos)
}

async fn delete_namespace(
    State(mgr): State<AppState>,
    Path(ns): Path<String>,
) -> Result<impl IntoResponse, ApiError> {
    mgr.delete_namespace(&ns).await?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
struct UpsertReq {
    documents: Vec<DocumentInput>,
}

#[derive(Deserialize)]
struct DocumentInput {
    id: u64,
    vector: Option<Vec<f32>>,
    #[serde(default)]
    attributes: HashMap<String, serde_json::Value>,
}

async fn upsert_documents(
    State(mgr): State<AppState>,
    Path(ns): Path<String>,
    Json(req): Json<UpsertReq>,
) -> Result<impl IntoResponse, ApiError> {
    let docs: Vec<Document> = req.documents.into_iter().map(|d| Document {
        id: d.id,
        vector: d.vector,
        attributes: d.attributes.into_iter()
            .map(|(k, v)| (k, AttributeValue::from_json(&v)))
            .collect(),
    }).collect();
    mgr.upsert(&ns, docs).await?;
    Ok(StatusCode::OK)
}

#[derive(Deserialize)]
struct DeleteReq {
    ids: Vec<u64>,
}

async fn delete_documents(
    State(mgr): State<AppState>,
    Path(ns): Path<String>,
    Json(req): Json<DeleteReq>,
) -> Result<impl IntoResponse, ApiError> {
    mgr.delete_docs(&ns, req.ids).await?;
    Ok(StatusCode::OK)
}

#[derive(Serialize)]
struct DocumentResp {
    id: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    vector: Option<Vec<f32>>,
    attributes: HashMap<String, serde_json::Value>,
}

impl From<Document> for DocumentResp {
    fn from(doc: Document) -> Self {
        Self {
            id: doc.id,
            vector: doc.vector,
            attributes: doc.attributes.into_iter()
                .map(|(k, v)| (k, v.to_json()))
                .collect(),
        }
    }
}

async fn get_document(
    State(mgr): State<AppState>,
    Path((ns, id)): Path<(String, u64)>,
) -> Result<impl IntoResponse, ApiError> {
    match mgr.get_document(&ns, id).await? {
        Some(doc) => Ok(Json(DocumentResp::from(doc)).into_response()),
        None => Ok(StatusCode::NOT_FOUND.into_response()),
    }
}

#[derive(Deserialize)]
struct QueryReq {
    vector: Vec<f32>,
    #[serde(default = "default_top_k")]
    top_k: usize,
    distance_metric: Option<DistanceMetric>,
    #[serde(default)]
    include_vectors: bool,
}

fn default_top_k() -> usize { 10 }

#[derive(Serialize)]
struct QueryResultResp {
    id: u64,
    dist: f32,
    #[serde(skip_serializing_if = "Option::is_none")]
    vector: Option<Vec<f32>>,
    attributes: HashMap<String, serde_json::Value>,
}

#[derive(Serialize)]
struct QueryResp {
    results: Vec<QueryResultResp>,
}

async fn query(
    State(mgr): State<AppState>,
    Path(ns): Path<String>,
    Json(req): Json<QueryReq>,
) -> Result<Json<QueryResp>, ApiError> {
    let results = mgr.query(
        &ns,
        req.vector,
        req.top_k,
        req.distance_metric,
        req.include_vectors,
    ).await?;
    let resp_results: Vec<QueryResultResp> = results.into_iter().map(|r| QueryResultResp {
        id: r.id,
        dist: r.dist,
        vector: r.vector,
        attributes: r.attributes.into_iter()
            .map(|(k, v)| (k, v.to_json()))
            .collect(),
    }).collect();
    Ok(Json(QueryResp { results: resp_results }))
}

struct ApiError(EngineError);

impl From<EngineError> for ApiError {
    fn from(e: EngineError) -> Self { Self(e) }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> axum::response::Response {
        let (status, msg) = match &self.0 {
            EngineError::NotFound(_) => (StatusCode::NOT_FOUND, self.0.to_string()),
            EngineError::AlreadyExists(_) => (StatusCode::CONFLICT, self.0.to_string()),
            EngineError::DimensionMismatch { .. } => (StatusCode::BAD_REQUEST, self.0.to_string()),
            EngineError::NoVector => (StatusCode::BAD_REQUEST, self.0.to_string()),
            EngineError::Validation(m) => (StatusCode::BAD_REQUEST, m.clone()),
            EngineError::Storage(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
        };
        let body = serde_json::json!({ "error": msg });
        (status, Json(body)).into_response()
    }
}
