//! 本地待发布：扫描一个目录里的视频和封面，记住页面上改过的投稿参数，再转码、投稿、加入合集。

use crate::server::errors::{AppError, AppResult};
use crate::server::infrastructure::connection_pool::ConnectionPool;
use crate::uploader::{cover_up, login_by_cookies, upload};
use biliup::uploader::bilibili::{Studio, Video};
use biliup::uploader::season::EpisodeAdd;
use error_stack::ResultExt;
use serde::Serialize;
use serde_json::{Value, json};
use sqlx::{Row, SqlitePool};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
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
    let (drafts, directory_error) = match prepare_directory(&directory) {
        Ok(dir) => {
            sync_directory(pool, &dir).await?;
            let drafts = drafts_in(pool, &dir).await?;
            (drafts, None)
        }
        Err(message) => (Vec::new(), Some(message)),
    };
    Ok(DraftList {
        directory: directory.to_string_lossy().into_owned(),
        directory_error,
        drafts,
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

pub async fn update(pool: &ConnectionPool, id: i64, patch: DraftUpdate) -> AppResult<Draft> {
    validate_update(&patch).map_err(AppError::Custom)?;
    let tags = serde_json::to_string(&patch.tags)
        .change_context_lazy(|| AppError::Custom("标签格式无效".into()))?;
    let changed = sqlx::query(
        "UPDATE local_drafts SET title=?, description=?, tags=?, tid=?, copyright=?, source=?, \
         is_only_self=?, section_id=?, user_id=?, title_locked=1, updated_at=? WHERE id=?",
    )
    .bind(&patch.title)
    .bind(&patch.description)
    .bind(tags)
    .bind(patch.tid)
    .bind(patch.copyright)
    .bind(&patch.source)
    .bind(patch.is_only_self)
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
    id: i64,
    move_after: bool,
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
    let status = if draft.needs_transcode { "transcoding" } else { "uploading" };
    let changed = sqlx::query(
        "UPDATE local_drafts SET status=?, error=NULL, updated_at=? \
         WHERE id=? AND status NOT IN ('transcoding','uploading')",
    )
    .bind(status)
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
    tokio::spawn(async move {
        if let Err(error) = run_publish(&pool, id, move_after).await {
            warn!(id, %error, "本地草稿投稿失败");
            let _ = mark_failed(&pool, id, &error).await;
        }
    });
    Ok(())
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

async fn run_publish(pool: &ConnectionPool, id: i64, move_after: bool) -> Result<(), String> {
    let draft = load_row(pool, id)
        .await
        .map_err(|error| error.to_string())?
        .ok_or_else(|| "草稿不存在".to_string())?;
    let video = if draft.needs_transcode {
        set_status(pool, id, "transcoding").await?;
        transcode(&draft).await?
    } else {
        PathBuf::from(&draft.video_path)
    };
    set_status(pool, id, "uploading").await?;
    let cookie = cookie_for(pool, draft.user_id.unwrap_or(0))
        .await
        .map_err(|error| error.to_string())?;
    let bili = login_by_cookies(cookie, None)
        .await
        .map_err(|error| error.to_string())?;
    let mut studio: Studio = serde_json::from_value(json!({
        "copyright": draft.copyright,
        "source": draft.source,
        "tid": draft.tid,
        "cover": draft.cover_path.clone().unwrap_or_default(),
        "title": draft.title,
        "desc": draft.description,
        "tag": draft.tags.join(","),
        "is_only_self": draft.is_only_self,
    }))
    .map_err(|error| format!("投稿参数无效：{error}"))?;
    cover_up(&mut studio, &bili)
        .await
        .map_err(|error| error.to_string())?;
    let videos: Vec<Video> = upload(std::slice::from_ref(&video), &bili, None, 3)
        .await
        .map_err(|error| error.to_string())?;
    studio.videos = videos;
    let submitted = bili
        .submit_by_app(&studio, None)
        .await
        .map_err(|error| error.to_string())?;
    if submitted.code != 0 {
        return Err(format!("投稿被拒绝：{submitted}"));
    }
    let data = submitted.data.unwrap_or(Value::Null);
    let bvid = data["bvid"].as_str().unwrap_or("").to_string();
    let aid = data["aid"].as_u64();
    let mut season_error = None;
    if let Some(section_id) = draft.section_id.filter(|id| *id > 0) {
        match add_season(&bili, section_id as u64, aid).await {
            Ok(()) => {}
            Err(error) => season_error = Some(format!("已投稿，加入合集失败：{error}")),
        }
    }
    if move_after {
        let _ = archive_files(pool, &draft).await;
    }
    sqlx::query(
        "UPDATE local_drafts SET status='submitted', bvid=?, error=?, updated_at=? WHERE id=?",
    )
    .bind(if bvid.is_empty() { None } else { Some(bvid) })
    .bind(season_error)
    .bind(now_secs())
    .bind(id)
    .execute(pool)
    .await
    .map_err(|error| error.to_string())?;
    Ok(())
}

async fn add_season(
    bili: &biliup::uploader::bilibili::BiliBili,
    section_id: u64,
    aid: Option<u64>,
) -> Result<(), String> {
    let aid = aid.ok_or("投稿结果里没有 aid")?;
    let view = bili
        .archive_view(aid)
        .await
        .map_err(|error| error.to_string())?;
    bili.add_to_season(section_id, &[EpisodeAdd::from(&view)])
        .await
        .map_err(|error| error.to_string())
}

async fn transcode(draft: &DraftRow) -> Result<PathBuf, String> {
    let source = PathBuf::from(&draft.video_path);
    let root = source
        .parent()
        .ok_or_else(|| "视频没有所在目录".to_string())?;
    let dir = root.join(".biliup-publish");
    std::fs::create_dir_all(&dir).map_err(|error| format!("创建转码目录失败：{error}"))?;
    let output = dir.join(format!("{}.mp4", draft.id));
    let ffmpeg = crate::tools::ffmpeg();
    let mut command = Command::new(ffmpeg);
    command
        .arg("-y")
        .arg("-i")
        .arg(&source)
        .args(video_encoder_args())
        .args(["-c:a", "aac", "-b:a", "192k", "-ar", "48000", "-ac", "2"])
        .args(["-movflags", "+faststart"])
        .arg(&output)
        .kill_on_drop(true);
    let finished = command
        .output()
        .await
        .map_err(|error| format!("启动 FFmpeg 失败：{error}"))?;
    if !finished.status.success() {
        let stderr = String::from_utf8_lossy(&finished.stderr);
        let tail: String = stderr.chars().rev().take(400).collect::<String>().chars().rev().collect();
        return Err(format!("转码失败：{tail}"));
    }
    Ok(output)
}

fn video_encoder_args() -> &'static [&'static str] {
    if cfg!(target_os = "macos") {
        &[
            "-c:v",
            "h264_videotoolbox",
            "-profile:v",
            "high",
            "-pix_fmt",
            "yuv420p",
            "-b:v",
            "8M",
        ]
    } else {
        &[
            "-c:v", "libx264", "-preset", "veryfast", "-crf", "20", "-pix_fmt", "yuv420p",
        ]
    }
}

async fn archive_files(pool: &ConnectionPool, draft: &DraftRow) -> Result<(), String> {
    let source = PathBuf::from(&draft.video_path);
    let root = source.parent().ok_or("视频没有所在目录")?;
    let published = root.join("published");
    std::fs::create_dir_all(&published).map_err(|error| error.to_string())?;
    let video_dest = unique_dest(&published, &source);
    std::fs::rename(&source, &video_dest).map_err(|error| error.to_string())?;
    let mut cover_dest = None;
    if let Some(cover) = &draft.cover_path {
        let cover = PathBuf::from(cover);
        if cover.is_file() {
            let dest = unique_dest(&published, &cover);
            std::fs::rename(&cover, &dest).map_err(|error| error.to_string())?;
            cover_dest = Some(dest.to_string_lossy().into_owned());
        }
    }
    sqlx::query("UPDATE local_drafts SET video_path=?, cover_path=? WHERE id=?")
        .bind(video_dest.to_string_lossy().into_owned())
        .bind(cover_dest)
        .bind(draft.id)
        .execute(pool)
        .await
        .map_err(|error| error.to_string())?;
    Ok(())
}

fn unique_dest(dir: &Path, source: &Path) -> PathBuf {
    let name = source
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "video.mp4".into());
    let candidate = dir.join(&name);
    if !candidate.exists() {
        return candidate;
    }
    let stem = source
        .file_stem()
        .map(|stem| stem.to_string_lossy().into_owned())
        .unwrap_or_else(|| "video".into());
    let ext = source
        .extension()
        .map(|ext| format!(".{}", ext.to_string_lossy()))
        .unwrap_or_default();
    dir.join(format!("{stem}-{}{ext}", now_secs()))
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
            "INSERT INTO local_drafts (video_path, video_bytes, video_mtime, cover_path, title, description, tags, tid, copyright, source, needs_transcode, video_codec, audio_codec, duration_ms, width, height, updated_at) \
             VALUES (?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?)",
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
    let needs_transcode = codec_needs_transcode(video_codec.as_deref(), audio_codec.as_deref());
    Some(Probe {
        video_codec,
        audio_codec,
        duration_ms,
        width,
        height,
        needs_transcode,
    })
}

fn codec_needs_transcode(video: Option<&str>, audio: Option<&str>) -> bool {
    let video_bad = !matches!(video, Some("h264" | "hevc" | "avc"));
    let audio_bad = matches!(audio, Some("opus" | "vorbis"));
    video_bad || audio_bad
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

const DRAFT_COLUMNS: &str = "SELECT id, video_path, video_bytes, video_mtime, cover_path, title, description, tags, tid, copyright, source, is_only_self, section_id, user_id, title_locked, status, bvid, error, needs_transcode, video_codec, audio_codec, duration_ms, width, height FROM local_drafts";
const DRAFT_SELECT: &str = "SELECT id, video_path, video_bytes, video_mtime, cover_path, title, description, tags, tid, copyright, source, is_only_self, section_id, user_id, title_locked, status, bvid, error, needs_transcode, video_codec, audio_codec, duration_ms, width, height FROM local_drafts WHERE id=?";

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

    #[test]
    fn av1_and_opus_need_transcode() {
        assert!(codec_needs_transcode(Some("av1"), Some("opus")));
        assert!(codec_needs_transcode(Some("h264"), Some("opus")));
        assert!(!codec_needs_transcode(Some("h264"), Some("aac")));
        assert!(!codec_needs_transcode(Some("hevc"), Some("aac")));
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
                section_id: Some(99),
                user_id: None,
            },
        )
        .await
        .unwrap();
        let second = list(&pool).await.unwrap();
        assert_eq!(second.drafts[0].title, "手工标题");
        assert_eq!(second.drafts[0].section_id, Some(99));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
