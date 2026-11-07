//! 本地待发布页面：列出目录里的视频、保存投稿参数、发布。
//!
//! 看列表和封面要 `file.view`，改参数、改目录、发布和读合集要 `upload.submit`。

use crate::server::infrastructure::connection_pool::ConnectionPool;
use crate::server::local_drafts::{self, Draft, DraftList, DraftReject, DraftUpdate, SeasonItem};
use axum::Json;
use axum::body::Body;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use serde::Deserialize;
use std::path::PathBuf;
use tracing::warn;

fn failure(error: impl std::fmt::Display) -> Response {
    warn!(%error, "本地待发布接口出错");
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        error.to_string(),
    )
        .into_response()
}

fn reject(error: DraftReject) -> Response {
    match error {
        DraftReject::NotFound => (StatusCode::NOT_FOUND, "草稿不存在").into_response(),
        DraftReject::Busy => (StatusCode::CONFLICT, "这条正在发布").into_response(),
        DraftReject::Invalid(message) => (StatusCode::BAD_REQUEST, message).into_response(),
    }
}

pub async fn list_drafts(State(pool): State<ConnectionPool>) -> Result<Json<DraftList>, Response> {
    local_drafts::list(&pool).await.map(Json).map_err(failure)
}

#[derive(Deserialize)]
pub struct DirectoryBody {
    directory: String,
}

pub async fn put_directory(
    State(pool): State<ConnectionPool>,
    Json(body): Json<DirectoryBody>,
) -> Result<Json<serde_json::Value>, Response> {
    let directory = local_drafts::set_directory(&pool, &body.directory)
        .await
        .map_err(|error| {
            let message = error.to_string();
            if message.contains("不存在") || message.contains("请填写") {
                (StatusCode::BAD_REQUEST, message).into_response()
            } else {
                failure(error)
            }
        })?;
    Ok(Json(serde_json::json!({ "directory": directory })))
}

#[derive(Deserialize)]
pub struct UpdateBody {
    title: String,
    description: String,
    tags: Vec<String>,
    tid: i64,
    copyright: i64,
    source: String,
    is_only_self: i64,
    section_id: Option<i64>,
    user_id: Option<i64>,
}

pub async fn put_draft(
    State(pool): State<ConnectionPool>,
    Path(id): Path<i64>,
    Json(body): Json<UpdateBody>,
) -> Result<Json<Draft>, Response> {
    local_drafts::update(
        &pool,
        id,
        DraftUpdate {
            title: body.title,
            description: body.description,
            tags: body.tags,
            tid: body.tid,
            copyright: body.copyright,
            source: body.source,
            is_only_self: body.is_only_self,
            section_id: body.section_id,
            user_id: body.user_id,
        },
    )
    .await
    .map(Json)
    .map_err(|error| {
        let message = error.to_string();
        if message.contains("不存在") {
            (StatusCode::NOT_FOUND, message).into_response()
        } else if message.contains("不能")
            || message.contains("请")
            || message.contains("至少")
            || message.contains("无效")
            || message.contains("转载")
        {
            (StatusCode::BAD_REQUEST, message).into_response()
        } else {
            failure(error)
        }
    })
}

pub async fn delete_draft(
    State(pool): State<ConnectionPool>,
    Path(id): Path<i64>,
) -> Result<StatusCode, Response> {
    local_drafts::dismiss(&pool, id).await.map_err(|error| {
        let message = error.to_string();
        if message.contains("不存在") {
            (StatusCode::NOT_FOUND, message).into_response()
        } else if message.contains("正在发布") {
            (StatusCode::CONFLICT, message).into_response()
        } else {
            failure(error)
        }
    })?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
pub struct PublishBody {
    #[serde(default)]
    move_after: bool,
}

pub async fn publish_draft(
    State(pool): State<ConnectionPool>,
    Path(id): Path<i64>,
    Json(body): Json<PublishBody>,
) -> Result<Json<serde_json::Value>, Response> {
    local_drafts::start_publish(&pool, id, body.move_after)
        .await
        .map_err(reject)?;
    Ok(Json(serde_json::json!({ "id": id, "status": "queued" })))
}

pub async fn draft_cover(
    State(pool): State<ConnectionPool>,
    Path(id): Path<i64>,
) -> Result<Response, Response> {
    let path = local_drafts::cover_file(&pool, id).await.map_err(|error| {
        let message = error.to_string();
        if message.contains("没有封面") || message.contains("不存在") {
            (StatusCode::NOT_FOUND, message).into_response()
        } else {
            failure(error)
        }
    })?;
    file_response(path).await
}

#[derive(Deserialize)]
pub struct SeasonQuery {
    user_id: i64,
}

pub async fn list_seasons(
    State(pool): State<ConnectionPool>,
    Query(query): Query<SeasonQuery>,
) -> Result<Json<Vec<SeasonItem>>, Response> {
    local_drafts::seasons(&pool, query.user_id)
        .await
        .map(Json)
        .map_err(failure)
}

async fn file_response(path: PathBuf) -> Result<Response, Response> {
    let bytes = tokio::fs::read(&path).await.map_err(|_| {
        (StatusCode::NOT_FOUND, "封面文件打不开".to_string()).into_response()
    })?;
    let mime: &'static str = match path
        .extension()
        .and_then(|ext| ext.to_str())
        .unwrap_or("")
        .to_ascii_lowercase()
        .as_str()
    {
        "png" => "image/png",
        "webp" => "image/webp",
        _ => "image/jpeg",
    };
    let mut headers = HeaderMap::new();
    headers.insert(header::CONTENT_TYPE, HeaderValue::from_static(mime));
    Ok((headers, Body::from(bytes)).into_response())
}
