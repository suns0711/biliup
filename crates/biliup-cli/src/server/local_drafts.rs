//! 本地待发布：扫描一个目录里的视频和封面，记住页面上改过的投稿参数，再投稿、加入合集。

use crate::server::common::upload_observation::{NewTask, TaskObservation, UploadObservations};
use crate::server::errors::{AppError, AppResult};
use crate::server::infrastructure::connection_pool::ConnectionPool;
use crate::uploader::{cover_up, login_by_cookies, upload};
use biliup::uploader::bilibili::{Studio, Vid, Video};
use biliup::uploader::season::EpisodeAdd;
use error_stack::ResultExt;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sqlx::{Row, SqlitePool};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::process::Command;
use tracing::warn;

const VIDEO_EXTS: &[&str] = &["mp4", "mkv", "mov", "webm", "flv", "ts", "m4v"];
const IMAGE_EXTS: &[&str] = &["jpg", "jpeg", "png", "webp"];

#[derive(Debug, Clone, Serialize)]
pub struct Draft {
    pub id: i64,
    pub file_name: String,
    pub has_cover: bool,
    pub title: String,
    pub description: String,
    pub tags: Vec<String>,
    pub tid: i64,
    pub copyright: i64,
    pub source: String,
    pub is_only_self: i64,
    pub statement_id: i64,
    pub no_reprint: i64,
    /// 0 生成更新推送，1 不生成。
    pub no_disturbance: i64,
    pub dtime: Option<i64>,
    /// 投稿成功的时间。定时公开的稿件仍以 dtime 为准。
    pub submitted_at: Option<i64>,
    pub section_id: Option<i64>,
    pub user_id: Option<i64>,
    pub status: String,
    pub bvid: Option<String>,
    pub error: Option<String>,
    pub needs_transcode: bool,
    pub video_codec: Option<String>,
    pub audio_codec: Option<String>,
    pub duration_ms: Option<i64>,
    pub width: Option<i64>,
    pub height: Option<i64>,
    pub bytes: i64,
}

#[derive(Debug, Clone, Serialize)]
pub struct DraftList {
    pub directory: String,
    pub directory_error: Option<String>,
    pub drafts: Vec<Draft>,
    pub directory_tags: Vec<DirectoryTag>,
    pub active_tag_id: Option<i64>,
}

#[derive(Debug, Clone, Serialize)]
pub struct DirectoryTag {
    pub id: i64,
    pub name: String,
    pub tid: Option<i64>,
    pub user_id: Option<i64>,
    pub section_id: Option<i64>,
    pub tags: Vec<String>,
    pub statement_id: Option<i64>,
}

#[derive(Debug, Clone)]
pub struct DirectoryTagInput {
    pub name: String,
    pub tid: Option<i64>,
    pub user_id: Option<i64>,
    pub section_id: Option<i64>,
    pub tags: Vec<String>,
    pub statement_id: Option<i64>,
}

#[derive(Debug, Clone)]
pub struct DraftUpdate {
    pub title: String,
    pub description: String,
    pub tags: Vec<String>,
    pub tid: i64,
    pub copyright: i64,
    pub source: String,
    pub is_only_self: i64,
    pub statement_id: i64,
    pub no_reprint: i64,
    /// 0 生成更新推送，1 不生成。
    pub no_disturbance: i64,
    pub dtime: Option<i64>,
    pub section_id: Option<i64>,
    pub user_id: Option<i64>,
}

#[derive(Debug, Clone, Serialize)]
pub struct SeasonSection {
    pub id: u64,
    pub title: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct SeasonItem {
    pub id: u64,
    pub title: String,
    pub sections: Vec<SeasonSection>,
}

#[derive(Debug)]
pub enum DraftReject {
    NotFound,
    Busy,
    Invalid(String),
}

impl DraftReject {
    fn invalid(message: impl Into<String>) -> Self {
        Self::Invalid(message.into())
    }
}

struct Inferred {
    title: String,
    description: String,
    source: String,
    copyright: i64,
    tid: i64,
    tags: Vec<String>,
}

struct Probe {
    video_codec: Option<String>,
    audio_codec: Option<String>,
    duration_ms: Option<i64>,
    width: Option<i64>,
    height: Option<i64>,
    needs_transcode: bool,
}

struct FoundFile {
    path: PathBuf,
    bytes: i64,
    mtime: i64,
    cover: Option<PathBuf>,
    inferred: Inferred,
}

pub async fn list(pool: &ConnectionPool) -> AppResult<DraftList> {
    let directory = load_directory(pool).await?;
    let (directory_tags, active_tag_id) = load_directory_tags(pool).await?;
    let active = directory_tags
        .iter()
        .find(|tag| Some(tag.id) == active_tag_id)
        .cloned();
    let (drafts, directory_error) = match prepare_directory(&directory) {
        Ok(dir) => {
            sync_directory(pool, &dir).await?;
            let mut drafts = drafts_in(pool, &dir).await?;
            let content = load_content_json(&dir);
            for draft in &mut drafts {
                apply_content_json(draft, &content);
                if let Some(tag) = &active {
                    if draft.status != "uploading" && draft.status != "transcoding" {
                        paint_directory_tag(draft, tag);
                    }
                }
            }
            (drafts, None)
        }
        Err(message) => (Vec::new(), Some(message)),
    };
    Ok(DraftList {
        directory: directory.to_string_lossy().into_owned(),
        directory_error,
        drafts,
        directory_tags,
        active_tag_id: active.as_ref().map(|tag| tag.id),
    })
}

pub async fn set_directory(pool: &ConnectionPool, directory: &str) -> AppResult<String> {
    let path = PathBuf::from(directory.trim());
    if path.as_os_str().is_empty() {
        return Err(AppError::Custom("请填写视频目录".into()).into());
    }
    let dir = prepare_directory(&path).map_err(AppError::Custom)?;
    let text = dir.to_string_lossy().into_owned();
    sqlx::query("UPDATE local_publish_settings SET directory = ? WHERE id = 1")
        .bind(&text)
        .execute(pool)
        .await
        .change_context_lazy(|| AppError::Custom("保存视频目录失败".into()))?;
    Ok(text)
}

pub async fn save_directory_tag(
    pool: &ConnectionPool,
    id: Option<i64>,
    input: DirectoryTagInput,
) -> AppResult<DirectoryTag> {
    let input = normalize_tag(input).map_err(AppError::Custom)?;
    let tags = serde_json::to_string(&input.tags)
        .change_context_lazy(|| AppError::Custom("标签格式无效".into()))?;
    let duplicate: Option<i64> = sqlx::query_scalar("SELECT id FROM local_directory_tags WHERE name=?")
        .bind(&input.name)
        .fetch_optional(pool)
        .await
        .change_context_lazy(|| AppError::Custom("读取目录标签失败".into()))?;
    if duplicate.is_some_and(|existing| Some(existing) != id) {
        return Err(AppError::Custom("标签名称已存在".into()).into());
    }
    let saved_id = if let Some(id) = id {
        let changed = sqlx::query(
            "UPDATE local_directory_tags SET name=?, tid=?, user_id=?, section_id=?, tags=?, statement_id=? WHERE id=?",
        )
        .bind(&input.name)
        .bind(input.tid)
        .bind(input.user_id)
        .bind(input.section_id)
        .bind(&tags)
        .bind(input.statement_id)
        .bind(id)
        .execute(pool)
        .await
        .change_context_lazy(|| AppError::Custom("保存目录标签失败".into()))?
        .rows_affected();
        if changed == 0 {
            return Err(AppError::Custom("目录标签不存在".into()).into());
        }
        id
    } else {
        sqlx::query_scalar(
            "INSERT INTO local_directory_tags (name, tid, user_id, section_id, tags, statement_id) VALUES (?,?,?,?,?,?) RETURNING id",
        )
        .bind(&input.name)
        .bind(input.tid)
        .bind(input.user_id)
        .bind(input.section_id)
        .bind(&tags)
        .bind(input.statement_id)
        .fetch_one(pool)
        .await
        .change_context_lazy(|| AppError::Custom("保存目录标签失败".into()))?
    };
    load_directory_tag(pool, saved_id)
        .await?
        .ok_or_else(|| AppError::Custom("目录标签不存在".into()).into())
}

pub async fn delete_directory_tag(pool: &ConnectionPool, id: i64) -> AppResult<()> {
    sqlx::query("UPDATE local_publish_settings SET active_tag_id=NULL WHERE id=1 AND active_tag_id=?")
        .bind(id)
        .execute(pool)
        .await
        .change_context_lazy(|| AppError::Custom("清除目录标签失败".into()))?;
    let changed = sqlx::query("DELETE FROM local_directory_tags WHERE id=?")
        .bind(id)
        .execute(pool)
        .await
        .change_context_lazy(|| AppError::Custom("删除目录标签失败".into()))?
        .rows_affected();
    if changed == 0 {
        return Err(AppError::Custom("目录标签不存在".into()).into());
    }
    Ok(())
}

pub async fn set_active_directory_tag(pool: &ConnectionPool, id: Option<i64>) -> AppResult<()> {
    if let Some(id) = id {
        let found = load_directory_tag(pool, id).await?;
        if found.is_none() {
            return Err(AppError::Custom("目录标签不存在".into()).into());
        }
    }
    sqlx::query("UPDATE local_publish_settings SET active_tag_id=? WHERE id=1")
        .bind(id)
        .execute(pool)
        .await
        .change_context_lazy(|| AppError::Custom("保存目录标签失败".into()))?;
    Ok(())
}

/// 把目录标签写进指定稿件。正在上传的稿件跳过。
pub async fn apply_directory_tag(
    pool: &ConnectionPool,
    id: i64,
    draft_ids: &[i64],
) -> AppResult<usize> {
    let tag = load_directory_tag(pool, id)
        .await?
        .ok_or_else(|| AppError::Custom("目录标签不存在".into()))?;
    set_active_directory_tag(pool, Some(id)).await?;
    let mut updated = 0;
    for draft_id in draft_ids {
        let Some(draft) = load_row(pool, *draft_id).await? else {
            continue;
        };
        if draft.status == "uploading" || draft.status == "transcoding" {
            continue;
        }
        let tid = tag.tid.filter(|value| *value > 0).unwrap_or(draft.tid);
        let user_id = tag.user_id.or(draft.user_id);
        let section_id = tag.section_id.or(draft.section_id);
        let tags = if tag.tags.is_empty() {
            draft.tags.clone()
        } else {
            tag.tags.clone()
        };
        let statement_id = tag.statement_id.unwrap_or(draft.statement_id);
        let copyright = if tag.statement_id.is_some() {
            if statement_id == -2 { 2 } else { 1 }
        } else {
            draft.copyright
        };
        let tags_json = serde_json::to_string(&tags)
            .change_context_lazy(|| AppError::Custom("标签格式无效".into()))?;
        sqlx::query(
            "UPDATE local_drafts SET tid=?, user_id=?, section_id=?, tags=?, statement_id=?, copyright=?, title_locked=1, updated_at=? WHERE id=?",
        )
        .bind(tid)
        .bind(user_id)
        .bind(section_id)
        .bind(tags_json)
        .bind(statement_id)
        .bind(copyright)
        .bind(now_secs())
        .bind(draft.id)
        .execute(pool)
        .await
        .change_context_lazy(|| AppError::Custom("套用目录标签失败".into()))?;
        updated += 1;
    }
    Ok(updated)
}

fn normalize_tag(mut input: DirectoryTagInput) -> Result<DirectoryTagInput, String> {
    input.name = input.name.trim().to_string();
    if input.name.is_empty() {
        return Err("请填写标签名称".into());
    }
    if input.name.chars().count() > 32 {
        return Err("标签名称太长".into());
    }
    if input.tid.is_some_and(|tid| tid <= 0) {
        input.tid = None;
    }
    input.tags = input
        .tags
        .into_iter()
        .map(|tag| tag.trim().to_string())
        .filter(|tag| !tag.is_empty())
        .take(12)
        .collect();
    if let Some(statement) = input.statement_id {
        if !matches!(statement, -2 | -1 | 1 | 2 | 3 | 4) {
            return Err("创作声明无效".into());
        }
    }
    Ok(input)
}

fn paint_directory_tag(draft: &mut Draft, tag: &DirectoryTag) {
    if let Some(tid) = tag.tid.filter(|tid| *tid > 0) {
        draft.tid = tid;
    }
    if let Some(user_id) = tag.user_id {
        draft.user_id = Some(user_id);
    }
    if let Some(section_id) = tag.section_id {
        draft.section_id = Some(section_id);
    }
    if !tag.tags.is_empty() {
        draft.tags = tag.tags.clone();
    }
    if let Some(statement_id) = tag.statement_id {
        draft.statement_id = statement_id;
        draft.copyright = if statement_id == -2 { 2 } else { 1 };
    }
}

async fn load_directory_tags(pool: &ConnectionPool) -> AppResult<(Vec<DirectoryTag>, Option<i64>)> {
    let active: Option<i64> =
        sqlx::query_scalar("SELECT active_tag_id FROM local_publish_settings WHERE id=1")
            .fetch_optional(pool)
            .await
            .change_context_lazy(|| AppError::Custom("读取目录标签失败".into()))?
            .flatten();
    let rows = sqlx::query(
        "SELECT id, name, tid, user_id, section_id, tags, statement_id FROM local_directory_tags ORDER BY id",
    )
    .fetch_all(pool)
    .await
    .change_context_lazy(|| AppError::Custom("读取目录标签失败".into()))?;
    Ok((rows.iter().map(directory_tag_from_row).collect(), active))
}

async fn load_directory_tag(pool: &ConnectionPool, id: i64) -> AppResult<Option<DirectoryTag>> {
    let row = sqlx::query(
        "SELECT id, name, tid, user_id, section_id, tags, statement_id FROM local_directory_tags WHERE id=?",
    )
    .bind(id)
    .fetch_optional(pool)
    .await
    .change_context_lazy(|| AppError::Custom("读取目录标签失败".into()))?;
    Ok(row.as_ref().map(directory_tag_from_row))
}

fn directory_tag_from_row(row: &sqlx::sqlite::SqliteRow) -> DirectoryTag {
    let tags: Vec<String> =
        serde_json::from_str(row.get::<String, _>("tags").as_str()).unwrap_or_default();
    DirectoryTag {
        id: row.get("id"),
        name: row.get("name"),
        tid: row.get("tid"),
        user_id: row.get("user_id"),
        section_id: row.get("section_id"),
        tags,
        statement_id: row.get("statement_id"),
    }
}

pub async fn update(pool: &ConnectionPool, id: i64, patch: DraftUpdate) -> AppResult<Draft> {
    validate_update(&patch).map_err(AppError::Custom)?;
    let tags = serde_json::to_string(&patch.tags)
        .change_context_lazy(|| AppError::Custom("标签格式无效".into()))?;
    let changed = sqlx::query(
        "UPDATE local_drafts SET title=?, description=?, tags=?, tid=?, copyright=?, source=?, \
         is_only_self=?, statement_id=?, no_reprint=?, no_disturbance=?, dtime=?, section_id=?, user_id=?, title_locked=1, updated_at=? WHERE id=?",
    )
    .bind(&patch.title)
    .bind(&patch.description)
    .bind(tags)
    .bind(patch.tid)
    .bind(patch.copyright)
    .bind(&patch.source)
    .bind(patch.is_only_self)
    .bind(patch.statement_id)
    .bind(patch.no_reprint)
    .bind(patch.no_disturbance)
    .bind(patch.dtime.filter(|time| *time > 0))
    .bind(patch.section_id)
    .bind(patch.user_id)
    .bind(now_secs())
    .bind(id)
    .execute(pool)
    .await
    .change_context_lazy(|| AppError::Custom("保存草稿失败".into()))?
    .rows_affected();
    if changed == 0 {
        return Err(AppError::Custom("草稿不存在".into()).into());
    }
    load(pool, id).await
}

pub async fn dismiss(pool: &ConnectionPool, id: i64) -> AppResult<()> {
    let status: Option<String> = sqlx::query_scalar("SELECT status FROM local_drafts WHERE id=?")
        .bind(id)
        .fetch_optional(pool)
        .await
        .change_context_lazy(|| AppError::Custom("读取草稿失败".into()))?;
    let Some(status) = status else {
        return Err(AppError::Custom("草稿不存在".into()).into());
    };
    if status == "transcoding" || status == "uploading" {
        return Err(AppError::Custom("这条正在发布，等结束后再移出".into()).into());
    }
    sqlx::query("DELETE FROM local_drafts WHERE id=?")
        .bind(id)
        .execute(pool)
        .await
        .change_context_lazy(|| AppError::Custom("移出草稿失败".into()))?;
    Ok(())
}

/// 删除视频目录里的视频和封面。未投稿的记录随文件一起移出列表；已投稿的记录保留。
pub async fn delete_files(pool: &ConnectionPool, id: i64) -> AppResult<()> {
    let draft = load_row(pool, id).await?;
    let Some(draft) = draft else {
        return Err(AppError::Custom("草稿不存在".into()).into());
    };
    if draft.status == "transcoding" || draft.status == "uploading" {
        return Err(AppError::Custom("这条正在发布，等结束后再删除".into()).into());
    }
    let directory = load_directory(pool).await?;
    let root = prepare_directory(&directory).map_err(AppError::Custom)?;
    remove_inside(&root, Path::new(&draft.video_path))?;
    if let Some(cover) = draft.cover_path.as_deref().filter(|path| !path.is_empty()) {
        remove_inside(&root, Path::new(cover))?;
    }
    if draft.status == "submitted" {
        sqlx::query("UPDATE local_drafts SET cover_path=NULL, updated_at=? WHERE id=?")
            .bind(now_secs())
            .bind(id)
            .execute(pool)
            .await
            .change_context_lazy(|| AppError::Custom("更新已投稿记录失败".into()))?;
        return Ok(());
    }
    dismiss(pool, id).await
}

fn remove_inside(root: &Path, path: &Path) -> AppResult<()> {
    if !path.exists() {
        return Ok(());
    }
    let file = path.canonicalize().change_context_lazy(|| {
        AppError::Custom(format!("无法读取 {}", path.display()))
    })?;
    if !file.starts_with(root) || !file.is_file() {
        return Err(AppError::Custom("无法删除不在视频目录里的文件".into()).into());
    }
    if file.file_name().and_then(|name| name.to_str()) == Some("content.json") {
        return Err(AppError::Custom("不能删除 content.json".into()).into());
    }
    std::fs::remove_file(&file).change_context_lazy(|| {
        AppError::Custom(format!("无法删除 {}", file.display()))
    })?;
    Ok(())
}

pub async fn cover_file(pool: &ConnectionPool, id: i64) -> AppResult<PathBuf> {
    let directory = load_directory(pool).await?;
    let root = prepare_directory(&directory).map_err(AppError::Custom)?;
    let path: Option<String> = sqlx::query_scalar("SELECT cover_path FROM local_drafts WHERE id=?")
        .bind(id)
        .fetch_optional(pool)
        .await
        .change_context_lazy(|| AppError::Custom("读取封面失败".into()))?;
    let Some(path) = path.filter(|path| !path.is_empty()) else {
        return Err(AppError::Custom("没有封面".into()).into());
    };
    let path = PathBuf::from(path);
    if !path.starts_with(&root) {
        return Err(AppError::Custom("封面不在视频目录里".into()).into());
    }
    Ok(path)
}

/// 校验通过后把状态改成发布中并在后台投稿。调用方立刻返回。
pub async fn start_publish(
    pool: &ConnectionPool,
    observations: &Arc<UploadObservations>,
    id: i64,
    delete_after: bool,
) -> Result<(), DraftReject> {
    let draft = load_row(pool, id).await.map_err(|error| DraftReject::invalid(error.to_string()))?;
    let Some(draft) = draft else {
        return Err(DraftReject::NotFound);
    };
    if draft.status == "transcoding" || draft.status == "uploading" {
        return Err(DraftReject::Busy);
    }
    validate_for_publish(&draft)?;
    cookie_for(pool, draft.user_id.expect("validate checks user"))
        .await
        .map_err(|error| DraftReject::invalid(error.to_string()))?;
    if !Path::new(&draft.video_path).is_file() {
        return Err(DraftReject::invalid("视频文件不存在"));
    }
    let changed = sqlx::query(
        "UPDATE local_drafts SET status=?, error=NULL, updated_at=? \
         WHERE id=? AND status NOT IN ('transcoding','uploading')",
    )
    .bind("uploading")
    .bind(now_secs())
    .bind(id)
    .execute(pool)
    .await
    .map_err(|error| DraftReject::invalid(error.to_string()))?
    .rows_affected();
    if changed == 0 {
        return Err(DraftReject::Busy);
    }
    let pool = pool.clone();
    let observations = observations.clone();
    let title = draft_title(&draft);
    let file_name = Path::new(&draft.video_path)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("video")
        .to_string();
    let file_bytes = u64::try_from(draft.bytes).ok();
    tokio::spawn(async move {
        let guard = observations.register(NewTask {
            source: "local",
            scheduler_group: "local",
            title: &title,
            streamer_name: None,
            session_id: None,
            template_name: None,
            fifo: false,
        });
        guard
            .observation()
            .prepare_files([(file_name, file_bytes)]);
        let result = guard
            .scope(async {
                guard.observation().phase("initializing");
                run_publish(&pool, id, delete_after).await
            })
            .await;
        if let Err(error) = &result {
            warn!(id, %error, "本地草稿投稿失败");
            let _ = mark_failed(&pool, id, error).await;
        }
        guard.finish_result(&result);
    });
    Ok(())
}

fn draft_title(draft: &DraftRow) -> String {
    let title = draft.title.trim();
    if !title.is_empty() {
        return title.to_string();
    }
    Path::new(&draft.video_path)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("本地发布")
        .to_string()
}

pub async fn seasons(pool: &ConnectionPool, user_id: i64) -> AppResult<Vec<SeasonItem>> {
    let cookie = cookie_for(pool, user_id).await?;
    let bili = login_by_cookies(cookie, None).await?;
    let mut items = Vec::new();
    for page in 1..=5 {
        let listed = bili
            .seasons(page, 30)
            .await
            .change_context_lazy(|| AppError::Custom("读取合集失败".into()))?;
        if listed.seasons.is_empty() {
            break;
        }
        let count = listed.seasons.len();
        for entry in listed.seasons {
            items.push(SeasonItem {
                id: entry.season.id,
                title: entry.season.title,
                sections: entry
                    .sections
                    .sections
                    .into_iter()
                    .map(|section| SeasonSection {
                        id: section.id,
                        title: section.title,
                    })
                    .collect(),
            });
        }
        if count < 30 {
            break;
        }
    }
    Ok(items)
}

async fn run_publish(pool: &ConnectionPool, id: i64, delete_after: bool) -> Result<(), String> {
    let draft = load_row(pool, id)
        .await
        .map_err(|error| error.to_string())?
        .ok_or_else(|| "草稿不存在".to_string())?;
    let video = PathBuf::from(&draft.video_path);
    set_status(pool, id, "uploading").await?;
    let cookie = cookie_for(pool, draft.user_id.unwrap_or(0))
        .await
        .map_err(|error| error.to_string())?;
    let bili = login_by_cookies(cookie, None)
        .await
        .map_err(|error| error.to_string())?;
    let mut body = json!({
        "copyright": draft.copyright,
        "source": draft.source,
        "tid": draft.tid,
        "cover": draft.cover_path.clone().unwrap_or_default(),
        "title": draft.title,
        "desc": draft.description,
        "tag": draft.tags.join(","),
        "is_only_self": draft.is_only_self,
        "no_reprint": draft.no_reprint,
        "no_disturbance": draft.no_disturbance,
        "creation_statement": { "id": draft.statement_id },
    });
    if let Some(dtime) = draft.dtime.filter(|time| *time > 0) {
        body["dtime"] = json!(dtime);
    }
    let mut studio: Studio = serde_json::from_value(body)
        .map_err(|error| format!("投稿参数无效：{error}"))?;
    cover_up(&mut studio, &bili)
        .await
        .map_err(|error| error.to_string())?;
    if let Some(task) = TaskObservation::current() {
        task.phase("uploading");
    }
    let videos: Vec<Video> = upload(std::slice::from_ref(&video), &bili, None, 3)
        .await
        .map_err(|error| error.to_string())?;
    studio.videos = videos;
    if let Some(task) = TaskObservation::current() {
        task.submitting().await;
    }
    let submitted = bili
        .submit_by_web(&studio, None)
        .await
        .map_err(|error| error.to_string())?;
    if submitted.code != 0 {
        let message = format!("投稿被拒绝：{submitted}");
        if let Some(task) = TaskObservation::current() {
            task.finish("failed", Some(&message), None);
        }
        return Err(message);
    }
    let data = submitted.data.unwrap_or(Value::Null);
    let bvid = data["bvid"].as_str().unwrap_or("").to_string();
    let aid = data["aid"].as_u64();
    let mut season_error = None;
    if let Some(section_id) = draft.section_id.filter(|id| *id > 0) {
        match add_season(&bili, section_id as u64, aid, &bvid).await {
            Ok(()) => {}
            Err(error) => season_error = Some(format!("已投稿，加入合集失败：{error}")),
        }
    }
    let mut file_error = None;
    if delete_after {
        if let Err(error) = delete_published_files(pool, &draft).await {
            file_error = Some(format!("已投稿，删除原文件失败：{error}"));
        }
    }
    let error_text = match (season_error, file_error) {
        (Some(season), Some(file)) => Some(format!("{season}；{file}")),
        (Some(season), None) => Some(season),
        (None, Some(file)) => Some(file),
        (None, None) => None,
    };
    sqlx::query(
        "UPDATE local_drafts SET status='submitted', bvid=?, error=?, submitted_at=COALESCE(submitted_at, ?), updated_at=? WHERE id=?",
    )
    .bind(if bvid.is_empty() {
        None
    } else {
        Some(bvid.as_str())
    })
    .bind(error_text.as_deref())
    .bind(now_secs())
    .bind(now_secs())
    .bind(id)
    .execute(pool)
    .await
    .map_err(|error| error.to_string())?;
    if let Some(task) = TaskObservation::current() {
        let published = if bvid.is_empty() {
            None
        } else {
            Some(bvid.as_str())
        };
        if let Some(message) = error_text.as_deref() {
            task.finish("succeeded", Some(message), published);
        } else if let Some(published) = published {
            task.bvid(published);
        }
    }
    Ok(())
}

async fn add_season(
    bili: &biliup::uploader::bilibili::BiliBili,
    section_id: u64,
    aid: Option<u64>,
    bvid: &str,
) -> Result<(), String> {
    let vid = if !bvid.is_empty() {
        Vid::Bvid(bvid.to_string())
    } else {
        Vid::Aid(aid.ok_or("投稿结果里没有稿件号")?)
    };
    // 刚投稿的稿件公开接口还查不到，隔几秒再向创作中心要一次。
    let mut last = String::new();
    for attempt in 0..4 {
        if attempt > 0 {
            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
        }
        match bili.creator_archive_view_by_vid(&vid).await {
            Ok(view) => {
                return bili
                    .add_to_season(section_id, &[EpisodeAdd::from(&view)])
                    .await
                    .map_err(|error| error.to_string());
            }
            Err(error) => {
                last = error.to_string();
                let missing = last.contains("-404")
                    || last.contains("62002")
                    || last.contains("啥都木有")
                    || last.contains("不可见");
                if !missing {
                    return Err(last);
                }
            }
        }
    }
    Err(last)
}

async fn delete_published_files(pool: &ConnectionPool, draft: &DraftRow) -> Result<(), String> {
    let directory = load_directory(pool).await.map_err(|error| error.to_string())?;
    let root = prepare_directory(&directory)?;
    remove_inside(&root, Path::new(&draft.video_path)).map_err(|error| error.to_string())?;
    if let Some(cover) = draft.cover_path.as_deref().filter(|path| !path.is_empty()) {
        remove_inside(&root, Path::new(cover)).map_err(|error| error.to_string())?;
    }
    Ok(())
}

async fn sync_directory(pool: &ConnectionPool, dir: &Path) -> AppResult<()> {
    let found = scan_files(dir).await?;
    let mut seen = Vec::with_capacity(found.len());
    for file in found {
        seen.push(file.path.to_string_lossy().into_owned());
        upsert(pool, &file).await?;
    }
    let existing = drafts_in(pool, dir).await?;
    for draft in existing {
        let row = load_row(pool, draft.id).await?;
        let Some(row) = row else { continue };
        let path = PathBuf::from(&row.video_path);
        if row.status == "transcoding" || row.status == "uploading" || row.status == "submitted" {
            continue;
        }
        if path.parent() == Some(dir) && !seen.iter().any(|item| item == &row.video_path) {
            sqlx::query("DELETE FROM local_drafts WHERE id=?")
                .bind(row.id)
                .execute(pool)
                .await
                .change_context_lazy(|| AppError::Custom("清理已消失的视频失败".into()))?;
        }
    }
    Ok(())
}

async fn upsert(pool: &ConnectionPool, file: &FoundFile) -> AppResult<()> {
    let path = file.path.to_string_lossy().into_owned();
    let existing = sqlx::query(
        "SELECT id, video_bytes, video_mtime, video_codec FROM local_drafts WHERE video_path=?",
    )
    .bind(&path)
    .fetch_optional(pool)
    .await
    .change_context_lazy(|| AppError::Custom("读取草稿失败".into()))?;
    let probe = match &existing {
        Some(row)
            if row.get::<i64, _>("video_bytes") == file.bytes
                && row.get::<i64, _>("video_mtime") == file.mtime
                && row.get::<Option<String>, _>("video_codec").is_some() =>
        {
            None
        }
        _ => Some(probe_file(&file.path).await),
    };
    let cover = file
        .cover
        .as_ref()
        .map(|path| path.to_string_lossy().into_owned());
    let tags = serde_json::to_string(&file.inferred.tags)
        .change_context_lazy(|| AppError::Custom("标签格式无效".into()))?;
    let probe = probe.unwrap_or_else(Probe::unknown);
    if existing.is_none() {
        sqlx::query(
            "INSERT INTO local_drafts (video_path, video_bytes, video_mtime, cover_path, title, description, tags, tid, copyright, source, needs_transcode, video_codec, audio_codec, duration_ms, width, height, no_disturbance, updated_at) \
             VALUES (?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?)",
        )
        .bind(&path)
        .bind(file.bytes)
        .bind(file.mtime)
        .bind(&cover)
        .bind(&file.inferred.title)
        .bind(&file.inferred.description)
        .bind(tags)
        .bind(file.inferred.tid)
        .bind(file.inferred.copyright)
        .bind(&file.inferred.source)
        .bind(if probe.needs_transcode { 1 } else { 0 })
        .bind(&probe.video_codec)
        .bind(&probe.audio_codec)
        .bind(probe.duration_ms)
        .bind(probe.width)
        .bind(probe.height)
        .bind(1_i64)
        .bind(now_secs())
        .execute(pool)
        .await
        .change_context_lazy(|| AppError::Custom("写入草稿失败".into()))?;
        return Ok(());
    }
    let refresh_probe = if probe.video_codec.is_some() || probe.audio_codec.is_some() {
        1
    } else {
        0
    };
    sqlx::query(
        "UPDATE local_drafts SET video_bytes=?, video_mtime=?, cover_path=?, \
         needs_transcode=CASE WHEN ? THEN ? ELSE needs_transcode END, \
         video_codec=CASE WHEN ? THEN ? ELSE video_codec END, \
         audio_codec=CASE WHEN ? THEN ? ELSE audio_codec END, \
         duration_ms=CASE WHEN ? THEN ? ELSE duration_ms END, \
         width=CASE WHEN ? THEN ? ELSE width END, \
         height=CASE WHEN ? THEN ? ELSE height END, \
         title=CASE WHEN title_locked=1 THEN title ELSE ? END, \
         description=CASE WHEN title_locked=1 THEN description ELSE ? END, \
         tags=CASE WHEN title_locked=1 THEN tags ELSE ? END, \
         tid=CASE WHEN title_locked=1 THEN tid ELSE ? END, \
         copyright=CASE WHEN title_locked=1 THEN copyright ELSE ? END, \
         source=CASE WHEN title_locked=1 THEN source ELSE ? END, \
         bvid=CASE WHEN status IN ('transcoding','uploading') THEN bvid WHEN video_mtime!=? OR video_bytes!=? THEN NULL ELSE bvid END, \
         error=CASE WHEN status IN ('transcoding','uploading') THEN error WHEN video_mtime!=? OR video_bytes!=? THEN NULL ELSE error END, \
         status=CASE WHEN status IN ('transcoding','uploading') THEN status WHEN video_mtime!=? OR video_bytes!=? THEN 'ready' ELSE status END, \
         updated_at=? WHERE video_path=?",
    )
    .bind(file.bytes)
    .bind(file.mtime)
    .bind(&cover)
    .bind(refresh_probe)
    .bind(if probe.needs_transcode { 1 } else { 0 })
    .bind(refresh_probe)
    .bind(&probe.video_codec)
    .bind(refresh_probe)
    .bind(&probe.audio_codec)
    .bind(refresh_probe)
    .bind(probe.duration_ms)
    .bind(refresh_probe)
    .bind(probe.width)
    .bind(refresh_probe)
    .bind(probe.height)
    .bind(&file.inferred.title)
    .bind(&file.inferred.description)
    .bind(&tags)
    .bind(file.inferred.tid)
    .bind(file.inferred.copyright)
    .bind(&file.inferred.source)
    .bind(file.mtime)
    .bind(file.bytes)
    .bind(file.mtime)
    .bind(file.bytes)
    .bind(file.mtime)
    .bind(file.bytes)
    .bind(now_secs())
    .bind(&path)
    .execute(pool)
    .await
    .change_context_lazy(|| AppError::Custom("更新草稿失败".into()))?;
    Ok(())
}

async fn scan_files(dir: &Path) -> AppResult<Vec<FoundFile>> {
    let mut videos = Vec::new();
    let mut images: HashMap<String, PathBuf> = HashMap::new();
    let entries = std::fs::read_dir(dir)
        .change_context_lazy(|| AppError::Custom(format!("无法读取目录 {}", dir.display())))?;
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        if name.starts_with('.') || name.starts_with("bili-h264") {
            continue;
        }
        let ext = path
            .extension()
            .and_then(|ext| ext.to_str())
            .unwrap_or("")
            .to_ascii_lowercase();
        let stem = path
            .file_stem()
            .and_then(|stem| stem.to_str())
            .unwrap_or("")
            .to_string();
        if IMAGE_EXTS.contains(&ext.as_str()) {
            images.entry(stem).or_insert(path);
        } else if VIDEO_EXTS.contains(&ext.as_str()) {
            videos.push(path);
        }
    }
    let mut found = Vec::new();
    for path in videos {
        let meta = std::fs::metadata(&path)
            .change_context_lazy(|| AppError::Custom(format!("无法读取 {}", path.display())))?;
        let stem = path
            .file_stem()
            .and_then(|stem| stem.to_str())
            .unwrap_or("")
            .to_string();
        let cover = cover_for(&stem, &images);
        found.push(FoundFile {
            inferred: infer_from_filename(&stem),
            path,
            bytes: meta.len() as i64,
            mtime: meta
                .modified()
                .ok()
                .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
                .map(|time| time.as_secs() as i64)
                .unwrap_or(0),
            cover,
        });
    }
    Ok(found)
}

fn cover_for(video_stem: &str, images: &HashMap<String, PathBuf>) -> Option<PathBuf> {
    if let Some(path) = images.get(video_stem) {
        return Some(path.clone());
    }
    let prefix = format!("{video_stem} ");
    images
        .iter()
        .filter(|(stem, _)| stem.starts_with(&prefix))
        .min_by_key(|(stem, _)| stem.len())
        .map(|(_, path)| path.clone())
}

#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
enum ContentTags {
    /// `#BeamNG #BeamNGdrive #汽车模拟`
    Text(String),
    List(Vec<String>),
}

impl Default for ContentTags {
    fn default() -> Self {
        Self::Text(String::new())
    }
}

#[derive(Debug, Default, Deserialize)]
struct ContentEntry {
    #[serde(default)]
    title: String,
    #[serde(default)]
    description: String,
    #[serde(default)]
    tags: ContentTags,
}

/// 上传目录里的 content.json。键是视频文件名或去掉扩展名的文件名。
fn load_content_json(dir: &Path) -> HashMap<String, ContentEntry> {
    let path = dir.join("content.json");
    let Ok(text) = std::fs::read_to_string(&path) else {
        return HashMap::new();
    };
    if text.trim().is_empty() {
        return HashMap::new();
    }
    match serde_json::from_str::<HashMap<String, ContentEntry>>(&text) {
        Ok(map) => map,
        Err(error) => {
            warn!(%error, path = %path.display(), "content.json 无法解析，改用文件名推断");
            HashMap::new()
        }
    }
}

fn content_entry<'a>(
    file_name: &str,
    content: &'a HashMap<String, ContentEntry>,
) -> Option<&'a ContentEntry> {
    if let Some(entry) = content.get(file_name) {
        return Some(entry);
    }
    Path::new(file_name)
        .file_stem()
        .and_then(|stem| stem.to_str())
        .and_then(|stem| content.get(stem))
}

/// 有非空的标题、简介或标签时覆盖页面上的对应项。空字段继续用原来的值。
fn apply_content_json(draft: &mut Draft, content: &HashMap<String, ContentEntry>) {
    let Some(entry) = content_entry(&draft.file_name, content) else {
        return;
    };
    let title = entry.title.trim();
    if !title.is_empty() {
        draft.title = title.to_string();
    }
    let description = entry.description.trim();
    if !description.is_empty() {
        draft.description = description.to_string();
    }
    let tags = tags_of(&entry.tags);
    if !tags.is_empty() {
        draft.tags = tags;
    }
}

fn tags_of(tags: &ContentTags) -> Vec<String> {
    match tags {
        ContentTags::Text(raw) => tags_from_hash_text(raw),
        ContentTags::List(items) => items
            .iter()
            .map(|tag| tag.trim().to_string())
            .filter(|tag| !tag.is_empty())
            .collect(),
    }
}

fn tags_from_hash_text(raw: &str) -> Vec<String> {
    raw.split('#')
        .map(|tag| tag.trim().to_string())
        .filter(|tag| !tag.is_empty())
        .collect()
}

fn infer_from_filename(stem: &str) -> Inferred {
    let mut work = stem.to_string();
    let mut source = String::new();
    if let Some(start) = work.rfind('[') {
        if let Some(end) = work[start + 1..].find(']') {
            let id = &work[start + 1..start + 1 + end];
            if is_youtube_id(id) {
                source = format!("https://www.youtube.com/watch?v={id}");
                work.replace_range(start..=start + end + 1, "");
            }
        }
    }
    let title = collapse_whitespace(
        &work
            .chars()
            .filter(|ch| kept_title_char(*ch))
            .collect::<String>(),
    );
    let title = truncate_chars(&title, 80);
    let beamng = title.to_ascii_lowercase().contains("beamng");
    let mut tags = Vec::new();
    if beamng {
        tags.push("BeamNG".into());
    }
    tags.push("视频".into());
    Inferred {
        description: if source.is_empty() {
            String::new()
        } else {
            format!("转载自 {source}")
        },
        copyright: if source.is_empty() { 1 } else { 2 },
        tid: if beamng { 17 } else { 21 },
        title,
        source,
        tags,
    }
}

fn is_youtube_id(id: &str) -> bool {
    id.len() == 11
        && id
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == '_' || ch == '-')
}

fn kept_title_char(ch: char) -> bool {
    ch.is_alphanumeric() || ch.is_whitespace() || "｜|#&-_.,，。、：:()（）[]【】/'+".contains(ch)
}

fn collapse_whitespace(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn truncate_chars(text: &str, max: usize) -> String {
    text.chars().take(max).collect()
}

async fn probe_file(path: &Path) -> Probe {
    let ffprobe = ffprobe_path();
    let result = Command::new(ffprobe)
        .args([
            "-v",
            "error",
            "-print_format",
            "json",
            "-show_streams",
            "-show_format",
        ])
        .arg(path)
        .output()
        .await;
    let Ok(output) = result else {
        return Probe::unknown();
    };
    if !output.status.success() {
        return Probe::unknown();
    }
    parse_probe(&output.stdout).unwrap_or_else(Probe::unknown)
}

fn parse_probe(bytes: &[u8]) -> Option<Probe> {
    let value: Value = serde_json::from_slice(bytes).ok()?;
    let streams = value.get("streams")?.as_array()?;
    let mut video_codec = None;
    let mut audio_codec = None;
    let mut width = None;
    let mut height = None;
    for stream in streams {
        let kind = stream.get("codec_type").and_then(Value::as_str);
        let codec = stream
            .get("codec_name")
            .and_then(Value::as_str)
            .map(str::to_string);
        match kind {
            Some("video") if video_codec.is_none() => {
                width = stream.get("width").and_then(Value::as_i64);
                height = stream.get("height").and_then(Value::as_i64);
                video_codec = codec;
            }
            Some("audio") if audio_codec.is_none() => audio_codec = codec,
            _ => {}
        }
    }
    let duration_ms = value
        .get("format")
        .and_then(|format| format.get("duration"))
        .and_then(Value::as_str)
        .and_then(|text| text.parse::<f64>().ok())
        .map(|seconds| (seconds * 1000.0) as i64);
    Some(Probe {
        video_codec,
        audio_codec,
        duration_ms,
        width,
        height,
        needs_transcode: false,
    })
}

impl Probe {
    fn unknown() -> Self {
        Self {
            video_codec: None,
            audio_codec: None,
            duration_ms: None,
            width: None,
            height: None,
            needs_transcode: false,
        }
    }
}

fn ffprobe_path() -> PathBuf {
    let ffmpeg = crate::tools::ffmpeg();
    if ffmpeg.components().count() <= 1 {
        return PathBuf::from(if cfg!(windows) { "ffprobe.exe" } else { "ffprobe" });
    }
    let mut path = ffmpeg;
    path.set_file_name(if cfg!(windows) { "ffprobe.exe" } else { "ffprobe" });
    path
}

fn validate_update(patch: &DraftUpdate) -> Result<(), String> {
    if patch.title.trim().is_empty() {
        return Err("标题不能为空".into());
    }
    if patch.title.chars().count() > 80 {
        return Err("标题不能超过 80 个字".into());
    }
    if patch.tags.is_empty() || patch.tags.len() > 12 {
        return Err("至少 1 个标签，最多 12 个".into());
    }
    if patch.tags.iter().any(|tag| tag.trim().is_empty() || tag.chars().count() > 20) {
        return Err("单个标签不能为空，且不超过 20 个字".into());
    }
    if patch.copyright != 1 && patch.copyright != 2 {
        return Err("版权只能是自制或转载".into());
    }
    if patch.copyright == 2 && patch.source.trim().is_empty() {
        return Err("转载稿件要填写来源".into());
    }
    if patch.is_only_self != 0 && patch.is_only_self != 1 {
        return Err("公开范围无效".into());
    }
    if !matches!(patch.statement_id, -2 | -1 | 1 | 2 | 3 | 4) {
        return Err("创作声明无效".into());
    }
    if patch.copyright == 1 && patch.statement_id == -2 {
        return Err("自制稿件不能选内容为转载".into());
    }
    if patch.no_reprint != 0 && patch.no_reprint != 1 {
        return Err("内容授权声明无效".into());
    }
    if patch.no_disturbance != 0 && patch.no_disturbance != 1 {
        return Err("更新推送设置无效".into());
    }
    if patch.copyright == 2 && patch.no_reprint == 1 {
        return Err("转载稿件不能加自制禁止转载".into());
    }
    if let Some(dtime) = patch.dtime.filter(|time| *time > 0) {
        let now = now_secs();
        if dtime < now + 4 * 3600 {
            return Err("请把定时发送设在 4 小时之后".into());
        }
        if dtime > now + 15 * 24 * 3600 {
            return Err("请把定时发送设在 15 天之内".into());
        }
    }
    if patch.tid <= 0 {
        return Err("请选择分区".into());
    }
    Ok(())
}

fn validate_for_publish(draft: &DraftRow) -> Result<(), DraftReject> {
    let patch = DraftUpdate {
        title: draft.title.clone(),
        description: draft.description.clone(),
        tags: draft.tags.clone(),
        tid: draft.tid,
        copyright: draft.copyright,
        source: draft.source.clone(),
        is_only_self: draft.is_only_self,
        statement_id: draft.statement_id,
        no_reprint: draft.no_reprint,
        no_disturbance: draft.no_disturbance,
        dtime: draft.dtime,
        section_id: draft.section_id,
        user_id: draft.user_id,
    };
    validate_update(&patch).map_err(DraftReject::invalid)?;
    if draft.user_id.is_none() {
        return Err(DraftReject::invalid("请先选择投稿账号"));
    }
    Ok(())
}

async fn cookie_for(pool: &ConnectionPool, user_id: i64) -> AppResult<PathBuf> {
    let value: Option<String> = sqlx::query_scalar(
        "SELECT value FROM configuration WHERE id=? AND key='bilibili-cookies'",
    )
    .bind(user_id)
    .fetch_optional(pool)
    .await
    .change_context_lazy(|| AppError::Custom("读取投稿账号失败".into()))?;
    let Some(value) = value else {
        return Err(AppError::Custom("投稿账号不存在".into()).into());
    };
    let path = PathBuf::from(value);
    if !path.is_file() {
        return Err(AppError::Custom(format!("找不到登录凭据 {}", path.display())).into());
    }
    Ok(path)
}

async fn load(pool: &ConnectionPool, id: i64) -> AppResult<Draft> {
    load_row(pool, id)
        .await?
        .map(Draft::from)
        .ok_or_else(|| AppError::Custom("草稿不存在".into()).into())
}

async fn load_row(pool: &SqlitePool, id: i64) -> AppResult<Option<DraftRow>> {
    let row = sqlx::query(DRAFT_SELECT)
        .bind(id)
        .fetch_optional(pool)
        .await
        .change_context_lazy(|| AppError::Custom("读取草稿失败".into()))?;
    Ok(row.as_ref().map(DraftRow::from_row))
}

async fn drafts_in(pool: &ConnectionPool, dir: &Path) -> AppResult<Vec<Draft>> {
    let prefix = format!("{}/", dir.to_string_lossy().trim_end_matches('/'));
    let rows = sqlx::query(&format!("{DRAFT_COLUMNS} WHERE video_path LIKE ? ORDER BY id DESC"))
        .bind(format!("{prefix}%"))
        .fetch_all(pool)
        .await
        .change_context_lazy(|| AppError::Custom("读取草稿列表失败".into()))?;
    Ok(rows.iter().map(DraftRow::from_row).map(Draft::from).collect())
}

async fn load_directory(pool: &ConnectionPool) -> AppResult<PathBuf> {
    let saved: String = sqlx::query_scalar("SELECT directory FROM local_publish_settings WHERE id=1")
        .fetch_one(pool)
        .await
        .change_context_lazy(|| AppError::Custom("读取视频目录失败".into()))?;
    if saved.trim().is_empty() {
        Ok(default_directory())
    } else {
        Ok(PathBuf::from(saved))
    }
}

fn default_directory() -> PathBuf {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("USERPROFILE").map(PathBuf::from))
        .unwrap_or_else(|| PathBuf::from("."));
    home.join("Downloads").join("youtube").join("upload")
}

fn prepare_directory(path: &Path) -> Result<PathBuf, String> {
    if !path.is_dir() {
        return Err(format!("目录不存在：{}", path.display()));
    }
    path.canonicalize()
        .map_err(|error| format!("无法打开目录 {}：{error}", path.display()))
}

async fn set_status(pool: &ConnectionPool, id: i64, status: &str) -> Result<(), String> {
    sqlx::query("UPDATE local_drafts SET status=?, updated_at=? WHERE id=?")
        .bind(status)
        .bind(now_secs())
        .bind(id)
        .execute(pool)
        .await
        .map_err(|error| error.to_string())?;
    Ok(())
}

async fn mark_failed(pool: &ConnectionPool, id: i64, error: &str) -> Result<(), String> {
    sqlx::query(
        "UPDATE local_drafts SET status='failed', error=?, updated_at=? \
         WHERE id=? AND status!='submitted'",
    )
    .bind(error)
    .bind(now_secs())
    .bind(id)
    .execute(pool)
    .await
    .map_err(|error| error.to_string())?;
    Ok(())
}

fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|time| time.as_secs() as i64)
        .unwrap_or(0)
}

const DRAFT_COLUMNS: &str = "SELECT id, video_path, video_bytes, video_mtime, cover_path, title, description, tags, tid, copyright, source, is_only_self, statement_id, no_reprint, no_disturbance, dtime, submitted_at, section_id, user_id, title_locked, status, bvid, error, needs_transcode, video_codec, audio_codec, duration_ms, width, height FROM local_drafts";
const DRAFT_SELECT: &str = "SELECT id, video_path, video_bytes, video_mtime, cover_path, title, description, tags, tid, copyright, source, is_only_self, statement_id, no_reprint, no_disturbance, dtime, submitted_at, section_id, user_id, title_locked, status, bvid, error, needs_transcode, video_codec, audio_codec, duration_ms, width, height FROM local_drafts WHERE id=?";

struct DraftRow {
    id: i64,
    video_path: String,
    bytes: i64,
    cover_path: Option<String>,
    title: String,
    description: String,
    tags: Vec<String>,
    tid: i64,
    copyright: i64,
    source: String,
    is_only_self: i64,
    statement_id: i64,
    no_reprint: i64,
    no_disturbance: i64,
    dtime: Option<i64>,
    submitted_at: Option<i64>,
    section_id: Option<i64>,
    user_id: Option<i64>,
    status: String,
    bvid: Option<String>,
    error: Option<String>,
    needs_transcode: bool,
    video_codec: Option<String>,
    audio_codec: Option<String>,
    duration_ms: Option<i64>,
    width: Option<i64>,
    height: Option<i64>,
}

impl DraftRow {
    fn from_row(row: &sqlx::sqlite::SqliteRow) -> Self {
        let tags: Vec<String> =
            serde_json::from_str(row.get::<String, _>("tags").as_str()).unwrap_or_default();
        Self {
            id: row.get("id"),
            video_path: row.get("video_path"),
            bytes: row.get("video_bytes"),
            cover_path: row.get("cover_path"),
            title: row.get("title"),
            description: row.get("description"),
            tags,
            tid: row.get("tid"),
            copyright: row.get("copyright"),
            source: row.get("source"),
            is_only_self: row.get("is_only_self"),
            statement_id: row.get("statement_id"),
            no_reprint: row.get("no_reprint"),
            no_disturbance: row.get("no_disturbance"),
            dtime: row.get("dtime"),
            submitted_at: row.get("submitted_at"),
            section_id: row.get("section_id"),
            user_id: row.get("user_id"),
            status: row.get("status"),
            bvid: row.get("bvid"),
            error: row.get("error"),
            needs_transcode: row.get::<i64, _>("needs_transcode") != 0,
            video_codec: row.get("video_codec"),
            audio_codec: row.get("audio_codec"),
            duration_ms: row.get("duration_ms"),
            width: row.get("width"),
            height: row.get("height"),
        }
    }
}

impl From<DraftRow> for Draft {
    fn from(row: DraftRow) -> Self {
        let file_name = Path::new(&row.video_path)
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| row.video_path.clone());
        Self {
            id: row.id,
            file_name,
            has_cover: row.cover_path.as_ref().is_some_and(|path| !path.is_empty()),
            title: row.title,
            description: row.description,
            tags: row.tags,
            tid: row.tid,
            copyright: row.copyright,
            source: row.source,
            is_only_self: row.is_only_self,
            statement_id: row.statement_id,
            no_reprint: row.no_reprint,
            no_disturbance: row.no_disturbance,
            dtime: row.dtime,
            submitted_at: row.submitted_at,
            section_id: row.section_id,
            user_id: row.user_id,
            status: row.status,
            bvid: row.bvid,
            error: row.error,
            needs_transcode: row.needs_transcode,
            video_codec: row.video_codec,
            audio_codec: row.audio_codec,
            duration_ms: row.duration_ms,
            width: row.width,
            height: row.height,
            bytes: row.bytes,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn content_json_overrides_only_filled_fields() {
        let mut draft = Draft {
            id: 1,
            file_name: "clip.mp4".into(),
            has_cover: false,
            title: "推断标题".into(),
            description: "推断简介".into(),
            tags: vec!["视频".into()],
            tid: 21,
            copyright: 1,
            source: String::new(),
            is_only_self: 0,
            statement_id: -1,
            no_reprint: 0,
            no_disturbance: 0,
            dtime: None,
            submitted_at: None,
            section_id: None,
            user_id: None,
            status: "ready".into(),
            bvid: None,
            error: None,
            needs_transcode: false,
            video_codec: None,
            audio_codec: None,
            duration_ms: None,
            width: None,
            height: None,
            bytes: 1,
        };
        let mut content = HashMap::new();
        content.insert(
            "clip".into(),
            ContentEntry {
                title: "文件标题".into(),
                description: String::new(),
                tags: ContentTags::Text("#BeamNG # BeamNGdrive  #汽车模拟 #物理碰撞".into()),
            },
        );
        apply_content_json(&mut draft, &content);
        assert_eq!(draft.title, "文件标题");
        assert_eq!(draft.description, "推断简介");
        assert_eq!(
            draft.tags,
            vec![
                "BeamNG".to_string(),
                "BeamNGdrive".to_string(),
                "汽车模拟".to_string(),
                "物理碰撞".to_string(),
            ]
        );

        let mut untouched = draft.clone();
        untouched.file_name = "other.mp4".into();
        untouched.title = "推断标题".into();
        apply_content_json(&mut untouched, &content);
        assert_eq!(untouched.title, "推断标题");

        let parsed: HashMap<String, ContentEntry> = serde_json::from_str(
            r#"{"clip.mp4":{"title":"数组标题","description":"数组简介","tags":[" BeamNG ","汽车模拟",""]}}"#,
        )
        .unwrap();
        apply_content_json(&mut draft, &parsed);
        assert_eq!(draft.title, "数组标题");
        assert_eq!(draft.description, "数组简介");
        assert_eq!(draft.tags, vec!["BeamNG".to_string(), "汽车模拟".to_string()]);
    }

    #[test]
    fn youtube_filename_becomes_a_reprint() {
        let inferred = infer_from_filename(
            "Cars and Trucks vs Speed Bumps #216 ｜ BeamNG Drive ｜🤜 Truck &Too [lrr3CQ9hGOY]",
        );
        assert_eq!(
            inferred.source,
            "https://www.youtube.com/watch?v=lrr3CQ9hGOY"
        );
        assert_eq!(inferred.copyright, 2);
        assert_eq!(inferred.tid, 17);
        assert!(inferred.tags.contains(&"BeamNG".to_string()));
        assert!(!inferred.title.contains('🤜'));
        assert!(inferred.title.contains("BeamNG Drive"));
        assert!(inferred.title.contains('#'));
    }

    #[test]
    fn cover_matches_exact_name_or_youtube_suffix() {
        let mut images = HashMap::new();
        images.insert("exact".into(), PathBuf::from("exact.jpg"));
        images.insert("clip 2".into(), PathBuf::from("clip 2.jpg"));
        images.insert("clip 2 extra".into(), PathBuf::from("clip 2 extra.jpg"));
        assert_eq!(
            cover_for("exact", &images).unwrap(),
            PathBuf::from("exact.jpg")
        );
        assert_eq!(
            cover_for("clip", &images).unwrap(),
            PathBuf::from("clip 2.jpg")
        );
    }

    #[tokio::test]
    async fn scan_keeps_a_manual_title_when_the_file_is_unchanged() {
        let dir = std::env::temp_dir().join(format!("biliup-local-drafts-{}", now_secs()));
        std::fs::create_dir_all(&dir).unwrap();
        let video = dir.join("Demo [abcdefghijk].mp4");
        let cover = dir.join("Demo [abcdefghijk] 2.jpg");
        std::fs::write(&video, b"video").unwrap();
        std::fs::write(&cover, b"cover").unwrap();
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        sqlx::migrate!("./migrations").run(&pool).await.unwrap();
        sqlx::query("UPDATE local_publish_settings SET directory=? WHERE id=1")
            .bind(dir.to_string_lossy().into_owned())
            .execute(&pool)
            .await
            .unwrap();

        let first = list(&pool).await.unwrap();
        assert_eq!(first.drafts.len(), 1);
        assert!(first.drafts[0].has_cover);
        assert_eq!(first.drafts[0].copyright, 2);
        let id = first.drafts[0].id;
        update(
            &pool,
            id,
            DraftUpdate {
                title: "手工标题".into(),
                description: "简介".into(),
                tags: vec!["标签".into()],
                tid: 21,
                copyright: 2,
                source: first.drafts[0].source.clone(),
                is_only_self: 0,
                statement_id: -2,
                no_reprint: 0,
                no_disturbance: 0,
                dtime: Some(now_secs() + 5 * 3600),
                section_id: Some(99),
                user_id: None,
            },
        )
        .await
        .unwrap();
        let second = list(&pool).await.unwrap();
        assert_eq!(second.drafts[0].title, "手工标题");
        assert_eq!(second.drafts[0].section_id, Some(99));
        assert_eq!(second.drafts[0].statement_id, -2);
        assert!(second.drafts[0].dtime.unwrap() > now_secs());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn delete_files_removes_the_video_and_cover_inside_the_directory() {
        let dir = std::env::temp_dir().join(format!("biliup-local-drafts-delete-{}", now_secs()));
        std::fs::create_dir_all(&dir).unwrap();
        let video = dir.join("clip.mp4");
        let cover = dir.join("clip.jpg");
        let notes = dir.join("content.json");
        std::fs::write(&video, b"video").unwrap();
        std::fs::write(&cover, b"cover").unwrap();
        std::fs::write(&notes, "{}").unwrap();
        let outside = std::env::temp_dir().join(format!("biliup-local-drafts-outside-{}.mp4", now_secs()));
        std::fs::write(&outside, b"keep").unwrap();
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        sqlx::migrate!("./migrations").run(&pool).await.unwrap();
        sqlx::query("UPDATE local_publish_settings SET directory=? WHERE id=1")
            .bind(dir.to_string_lossy().into_owned())
            .execute(&pool)
            .await
            .unwrap();

        let listed = list(&pool).await.unwrap();
        let id = listed.drafts[0].id;
        delete_files(&pool, id).await.unwrap();
        assert!(!video.exists());
        assert!(!cover.exists());
        assert!(notes.exists());
        assert!(list(&pool).await.unwrap().drafts.is_empty());

        std::fs::write(&video, b"video").unwrap();
        std::fs::write(&cover, b"cover").unwrap();
        let submitted_id = list(&pool).await.unwrap().drafts[0].id;
        sqlx::query("UPDATE local_drafts SET status='submitted', bvid='BV1test' WHERE id=?")
            .bind(submitted_id)
            .execute(&pool)
            .await
            .unwrap();
        delete_files(&pool, submitted_id).await.unwrap();
        assert!(!video.exists());
        assert!(!cover.exists());
        let kept = list(&pool).await.unwrap();
        assert_eq!(kept.drafts.len(), 1);
        assert_eq!(kept.drafts[0].status, "submitted");
        assert_eq!(kept.drafts[0].bvid.as_deref(), Some("BV1test"));
        assert!(!kept.drafts[0].has_cover);

        sqlx::query(
            "INSERT INTO local_drafts (video_path, video_bytes, video_mtime, title, description, tags, updated_at) VALUES (?,?,?,?,?,?,?)",
        )
        .bind(outside.to_string_lossy().into_owned())
        .bind(4_i64)
        .bind(1_i64)
        .bind("外面")
        .bind("")
        .bind("[]")
        .bind(now_secs())
        .execute(&pool)
        .await
        .unwrap();
        let outsider = sqlx::query_scalar::<_, i64>("SELECT id FROM local_drafts WHERE title='外面'")
            .fetch_one(&pool)
            .await
            .unwrap();
        let rejected = delete_files(&pool, outsider).await.unwrap_err();
        assert!(rejected.to_string().contains("视频目录"));
        assert!(outside.exists());

        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_file(&outside);
    }

    #[tokio::test]
    async fn directory_tag_sets_partition_statement_and_tags() {
        let dir = std::env::temp_dir().join(format!("biliup-local-drafts-tag-{}", now_secs()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("clip.mp4"), b"video").unwrap();
        std::fs::write(
            dir.join("content.json"),
            r#"{"clip.mp4":{"title":"文件标题","description":"文件简介","tags":["原来的"]}}"#,
        )
        .unwrap();
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        sqlx::migrate!("./migrations").run(&pool).await.unwrap();
        sqlx::query("UPDATE local_publish_settings SET directory=? WHERE id=1")
            .bind(dir.to_string_lossy().into_owned())
            .execute(&pool)
            .await
            .unwrap();

        let listed = list(&pool).await.unwrap();
        assert_eq!(listed.drafts[0].tags, vec!["原来的".to_string()]);
        assert_eq!(listed.drafts[0].no_disturbance, 1);
        let id = listed.drafts[0].id;
        let tag = save_directory_tag(
            &pool,
            None,
            DirectoryTagInput {
                name: "BeamNG".into(),
                tid: Some(258),
                user_id: Some(7),
                section_id: Some(99),
                tags: vec![" 预设 ".into(), "".into()],
                statement_id: Some(-1),
            },
        )
        .await
        .unwrap();
        apply_directory_tag(&pool, tag.id, &[id]).await.unwrap();
        let painted = list(&pool).await.unwrap();
        assert_eq!(painted.active_tag_id, Some(tag.id));
        assert_eq!(painted.drafts[0].title, "文件标题");
        assert_eq!(painted.drafts[0].tid, 258);
        assert_eq!(painted.drafts[0].user_id, Some(7));
        assert_eq!(painted.drafts[0].section_id, Some(99));
        assert_eq!(painted.drafts[0].statement_id, -1);
        assert_eq!(painted.drafts[0].tags, vec!["预设".to_string()]);

        sqlx::query("UPDATE local_drafts SET status='uploading' WHERE id=?")
            .bind(id)
            .execute(&pool)
            .await
            .unwrap();
        let other = save_directory_tag(
            &pool,
            None,
            DirectoryTagInput {
                name: "别的".into(),
                tid: Some(21),
                user_id: None,
                section_id: None,
                tags: vec!["不会套上".into()],
                statement_id: Some(1),
            },
        )
        .await
        .unwrap();
        let updated = apply_directory_tag(&pool, other.id, &[id]).await.unwrap();
        assert_eq!(updated, 0);
        let stored: i64 = sqlx::query_scalar("SELECT tid FROM local_drafts WHERE id=?")
            .bind(id)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(stored, 258);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
