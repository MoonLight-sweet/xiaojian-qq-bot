-- 独立的热梗记忆表（与群聊记忆 memories 表彻底分离）
CREATE TABLE IF NOT EXISTS hot_memes (
    meme_ref    TEXT PRIMARY KEY,
    bot_id      TEXT NOT NULL,
    group_id    TEXT NOT NULL,
    content     TEXT NOT NULL,
    source      TEXT NOT NULL DEFAULT 'trending_sync',
    confidence  REAL NOT NULL DEFAULT 0.9,
    created_at  INTEGER NOT NULL,
    updated_at  INTEGER NOT NULL,
    archived_at INTEGER
);

CREATE INDEX IF NOT EXISTS idx_hot_memes_group_active
    ON hot_memes(bot_id, group_id, archived_at, updated_at DESC);

-- 将原先混入 memories 表的热梗记忆（kind = 'meme' 或 origin 为热梗同步来源）迁移至 hot_memes 表并从 memories 表移除
INSERT OR IGNORE INTO hot_memes (
    meme_ref, bot_id, group_id, content, source, confidence, created_at, updated_at, archived_at
)
SELECT
    memory_ref, bot_id, group_id, content, origin, confidence, created_at, updated_at, archived_at
FROM memories
WHERE kind = 'meme' OR origin IN ('trending_sync', 'heybox_sync', 'heybox_lingo');

DELETE FROM memories
WHERE kind = 'meme' OR origin IN ('trending_sync', 'heybox_sync', 'heybox_lingo');
