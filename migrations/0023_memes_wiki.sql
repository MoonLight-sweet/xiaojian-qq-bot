-- 梗百科/黑话词典表 (Memes table, 借鉴自 dsh-memewiki / cnmeme.wiki)
CREATE TABLE IF NOT EXISTS memes (
    title       TEXT PRIMARY KEY,
    slug        TEXT,
    tags        TEXT,
    usage       TEXT,
    origin      TEXT,
    other       TEXT,
    summary     TEXT,
    aliases     TEXT,
    updated_at  INTEGER NOT NULL DEFAULT 0
);

CREATE INDEX IF NOT EXISTS idx_memes_slug ON memes(slug);
