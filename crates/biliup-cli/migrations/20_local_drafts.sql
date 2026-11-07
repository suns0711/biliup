CREATE TABLE local_publish_settings (
    id INTEGER PRIMARY KEY CHECK (id = 1),
    directory TEXT NOT NULL DEFAULT ''
);

INSERT INTO local_publish_settings (id, directory) VALUES (1, '');

CREATE TABLE local_drafts (
    id INTEGER PRIMARY KEY,
    video_path TEXT NOT NULL UNIQUE,
    video_bytes INTEGER NOT NULL,
    video_mtime INTEGER NOT NULL,
    cover_path TEXT,
    title TEXT NOT NULL DEFAULT '',
    description TEXT NOT NULL DEFAULT '',
    tags TEXT NOT NULL DEFAULT '[]',
    tid INTEGER NOT NULL DEFAULT 17,
    copyright INTEGER NOT NULL DEFAULT 1,
    source TEXT NOT NULL DEFAULT '',
    is_only_self INTEGER NOT NULL DEFAULT 0,
    section_id INTEGER,
    user_id INTEGER,
    title_locked INTEGER NOT NULL DEFAULT 0,
    status TEXT NOT NULL DEFAULT 'ready',
    bvid TEXT,
    error TEXT,
    needs_transcode INTEGER NOT NULL DEFAULT 0,
    video_codec TEXT,
    audio_codec TEXT,
    duration_ms INTEGER,
    width INTEGER,
    height INTEGER,
    updated_at INTEGER NOT NULL
);
