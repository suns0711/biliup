UPDATE local_drafts SET no_disturbance = 1 WHERE no_disturbance = 0;

CREATE TABLE local_directory_tags (
    id INTEGER PRIMARY KEY,
    name TEXT NOT NULL UNIQUE,
    tid INTEGER,
    user_id INTEGER,
    section_id INTEGER,
    tags TEXT NOT NULL DEFAULT '[]',
    statement_id INTEGER
);

ALTER TABLE local_publish_settings ADD COLUMN active_tag_id INTEGER;
