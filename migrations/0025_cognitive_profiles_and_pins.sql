-- 认知记忆三层架构（借鉴 69gg/Undefined 认知记忆与后台史官侧写 + MemPalace 人物侧翼分房）：
-- 第一层：置顶备忘录（pinned_memories），每轮固定注入
CREATE TABLE IF NOT EXISTS pinned_memories (
    pin_ref     TEXT PRIMARY KEY,
    bot_id      TEXT NOT NULL,
    group_id    TEXT NOT NULL,
    content     TEXT NOT NULL,
    created_at  INTEGER NOT NULL,
    updated_at  INTEGER NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_pinned_memories_group
    ON pinned_memories(bot_id, group_id, updated_at DESC);

-- 第二层：群友四段式结构化人物侧写档案（member_profiles，对应人物侧翼 Wing）
CREATE TABLE IF NOT EXISTS member_profiles (
    bot_id            TEXT NOT NULL,
    group_id          TEXT NOT NULL,
    user_id           TEXT NOT NULL,
    display_name      TEXT NOT NULL DEFAULT '',
    identity_tags     TEXT NOT NULL DEFAULT '',
    interests         TEXT NOT NULL DEFAULT '',
    personality_notes TEXT NOT NULL DEFAULT '',
    recent_status     TEXT NOT NULL DEFAULT '',
    source_msg_id     INTEGER NOT NULL DEFAULT 0,
    updated_at        INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (bot_id, group_id, user_id)
);

CREATE INDEX IF NOT EXISTS idx_member_profiles_group
    ON member_profiles(bot_id, group_id, updated_at DESC);

-- 将已有的群成员与现有群聊记忆（fact / preference / event）聚合初始化为群友侧写档案
INSERT OR IGNORE INTO member_profiles (
    bot_id, group_id, user_id, display_name, identity_tags, interests, personality_notes, recent_status, source_msg_id, updated_at
)
SELECT
    gm.bot_id,
    gm.group_id,
    gm.user_id,
    COALESCE(
        (SELECT sa.alias FROM speaker_aliases sa WHERE sa.bot_id = gm.bot_id AND sa.group_id = gm.group_id AND sa.user_id = gm.user_id LIMIT 1),
        (SELECT msg.display_name FROM group_messages msg WHERE msg.bot_id = gm.bot_id AND msg.group_id = gm.group_id AND msg.sender_id = gm.user_id AND msg.display_name IS NOT NULL AND msg.display_name != '' ORDER BY msg.id DESC LIMIT 1),
        gm.speaker_ref
    ) AS display_name,
    COALESCE(
        (SELECT GROUP_CONCAT(content, '；') FROM (SELECT content FROM memories m WHERE m.bot_id = gm.bot_id AND m.group_id = gm.group_id AND m.subject_user_id = gm.user_id AND m.archived_at IS NULL AND m.kind = 'fact' ORDER BY m.updated_at DESC LIMIT 3)),
        ''
    ) AS identity_tags,
    COALESCE(
        (SELECT GROUP_CONCAT(content, '；') FROM (SELECT content FROM memories m WHERE m.bot_id = gm.bot_id AND m.group_id = gm.group_id AND m.subject_user_id = gm.user_id AND m.archived_at IS NULL AND m.kind = 'preference' ORDER BY m.updated_at DESC LIMIT 3)),
        ''
    ) AS interests,
    '' AS personality_notes,
    COALESCE(
        (SELECT GROUP_CONCAT(content, '；') FROM (SELECT content FROM memories m WHERE m.bot_id = gm.bot_id AND m.group_id = gm.group_id AND m.subject_user_id = gm.user_id AND m.archived_at IS NULL AND m.kind = 'event' ORDER BY m.updated_at DESC LIMIT 3)),
        ''
    ) AS recent_status,
    0 AS source_msg_id,
    gm.last_seen_at AS updated_at
FROM group_members gm;
