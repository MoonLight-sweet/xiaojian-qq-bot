//! 梗百科/网络黑话词典（Meme Wiki）模块
//! 移植并借鉴自 dsh-memewiki (thawline) 与 cnmeme.wiki。
//! 具备本地 6700+ 离线词典快速检索、前置自动预查问梗注入、以及 MCP 工具调用能力。

use sqlx::{Row, SqlitePool};

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct MemeRecord {
    pub title: String,
    pub slug: Option<String>,
    pub tags: Option<String>,
    pub usage: Option<String>,
    pub origin: Option<String>,
    pub other: Option<String>,
    pub summary: Option<String>,
    pub aliases: Option<String>,
}

impl MemeRecord {
    /// 渲染为注入大模型 Prompt 的精炼参考段落
    pub fn render_context(&self) -> String {
        let mut out = String::new();
        out.push_str(&format!("【梗百科·知识库】：{}\n", self.title));
        if let Some(aliases) = &self.aliases {
            if !aliases.trim().is_empty() {
                out.push_str(&format!("- 常见别名/缩写：{}\n", aliases.trim()));
            }
        }
        if let Some(summary) = &self.summary {
            if !summary.trim().is_empty() {
                out.push_str(&format!("- 释义：{}\n", summary.trim()));
            }
        }
        if let Some(origin) = &self.origin {
            if !origin.trim().is_empty() {
                out.push_str(&format!("- 梗出处：{}\n", origin.trim()));
            }
        }
        if let Some(usage) = &self.usage {
            if !usage.trim().is_empty() {
                out.push_str(&format!("- 怎么接/怎么用：{}\n", usage.trim()));
            }
        }
        out.push_str("【接梗指引】：查到词条请自然融入对话或先接梗，切勿生硬背诵词条或科普教科书式展开；若对方明确询问出处方可简述。");
        out
    }
}

/// 判定用户消息是否在提问一个网络梗或词汇含义
/// 匹配模式涵盖：「XX是什么梗」「啥意思」「什么出处」「怎么用」「怎么接」以及单短词
pub fn extract_query(text: &str) -> Option<String> {
    let t = clean_text(text);
    if t.is_empty() {
        return None;
    }

    // 1. 匹配典型的问梗句式
    const PATTERNS: [&str; 12] = [
        "是什么梗", "啥梗", "什么来头", "什么来历",
        "什么出处", "什么意思", "啥意思", "怎么用",
        "怎么接", "怎么玩", "如何用", "是啥意思",
    ];

    for pat in PATTERNS {
        if let Some(idx) = t.find(pat) {
            let mut head = t[..idx].trim();
            // 剥离标点符号
            head = head.trim_matches(|c: char| matches!(c, ' ' | '，' | ',' | '。' | '！' | '!' | '？' | '?'));
            head = head.trim_start_matches("是").trim_end_matches("是");
            head = head.trim_end_matches("这个梗").trim_end_matches("那个梗").trim_end_matches("这梗").trim_end_matches("那梗");
            head = head.trim_end_matches("这个").trim_end_matches("那个");
            head = head.trim_end_matches("到底").trim_end_matches("究竟");
            let head = head.trim();

            if is_stop_word(head) {
                return None;
            }
            if head.chars().count() >= 2 && head.chars().count() <= 15 {
                return Some(head.to_string());
            }
            return None;
        }
    }

    // 2. 裸词模式：两到十个字的词组，且不是闲聊/功能词
    let char_count = t.chars().count();
    if (2..=10).contains(&char_count) && !is_stop_word(&t) && !is_chat_phrase(&t) {
        // 排除疑问词构成的整句
        const SENTENCE_PARTICLES: [&str; 11] = [
            "怎么样", "怎么", "为什么", "能不能", "是不是",
            "了吗", "了没", "一下", "你说", "要不要", "有没有",
        ];
        if SENTENCE_PARTICLES.iter().any(|p| t.contains(p)) {
            return None;
        }
        return Some(t);
    }

    None
}

/// 清洗消息文本中的 @ 与常见机器人唤醒词
fn clean_text(text: &str) -> String {
    let mut t = text.trim();
    // 剥离 @ 开头
    while t.starts_with('@') {
        if let Some(space_idx) = t.find(' ') {
            t = t[space_idx..].trim();
        } else {
            break;
        }
    }
    const WAKE_WORDS: [&str; 6] = ["小简", "Gemini", "gemini", "小猫", "猫猫", "bot"];
    for w in WAKE_WORDS {
        if let Some(stripped) = t.strip_prefix(w) {
            t = stripped.trim().trim_matches(|c: char| matches!(c, ' ' | '，' | ',' | '。' | '！' | '!' | '？' | '?' | ':' | '：'));
            break;
        }
    }
    t.trim().to_string()
}

fn is_stop_word(word: &str) -> bool {
    const STOPS: [&str; 25] = [
        "这", "那", "它", "他", "她", "这个", "那个", "你", "我", "谁", "什么", "哪",
        "在吗", "在不在", "哈哈", "哈哈哈", "笑死", "好的", "好吧", "晚安", "早安",
        "睡觉", "吃饭", "干嘛", "没事",
    ];
    STOPS.contains(&word)
}

fn is_chat_phrase(word: &str) -> bool {
    const PHRASES: [&str; 12] = [
        "我是谁", "今天吃什么", "今天看什么", "几点了", "天气怎么样",
        "测试一下", "你在干嘛", "晚饭吃什么", "午饭吃什么", "早上好", "晚安啦", "笑死我了",
    ];
    PHRASES.contains(&word)
}

/// 在 SQLite 中检索梗百科词条（支持精确、别名、前缀与包含匹配）
pub async fn search_meme(pool: &SqlitePool, query: &str) -> Option<MemeRecord> {
    let q = query.trim();
    if q.is_empty() {
        return None;
    }

    // 1. 完全精确匹配
    if let Ok(Some(row)) = sqlx::query(
        "SELECT title, slug, tags, usage, origin, other, summary, aliases FROM memes WHERE title = ? LIMIT 1"
    )
    .bind(q)
    .fetch_optional(pool)
    .await
    {
        return Some(row_to_meme(row));
    }

    // 2. 别名/缩写匹配（例如 kskbl -> 康神开播了，或纯别名比对）
    if let Ok(Some(row)) = sqlx::query(
        "SELECT title, slug, tags, usage, origin, other, summary, aliases FROM memes WHERE aliases IS NOT NULL AND instr(',' || lower(aliases) || ',', ',' || lower(?) || ',') > 0 LIMIT 1"
    )
    .bind(q)
    .fetch_optional(pool)
    .await
    {
        return Some(row_to_meme(row));
    }

    // 3. 标题前缀匹配（取标题最短的最精准）
    let pattern = format!("{q}%");
    if let Ok(Some(row)) = sqlx::query(
        "SELECT title, slug, tags, usage, origin, other, summary, aliases FROM memes WHERE lower(title) LIKE lower(?) ORDER BY length(title) ASC LIMIT 1"
    )
    .bind(&pattern)
    .fetch_optional(pool)
    .await
    {
        return Some(row_to_meme(row));
    }

    // 4. 查询词里包含梗标题（例如输入“去码头整点薯条是什么梗”，命中“去码头整点薯条”）
    if let Ok(Some(row)) = sqlx::query(
        "SELECT title, slug, tags, usage, origin, other, summary, aliases FROM memes WHERE length(title) >= 2 AND instr(?, title) > 0 ORDER BY length(title) DESC LIMIT 1"
    )
    .bind(q)
    .fetch_optional(pool)
    .await
    {
        return Some(row_to_meme(row));
    }

    // 5. 梗标题里包含查询词（反向模糊搜索，中文 >= 3 字）
    let is_short_ascii = q.len() >= 2 && q.chars().all(|c| c.is_ascii_alphanumeric());
    if q.chars().count() >= 3 || is_short_ascii {
        if let Ok(Some(row)) = sqlx::query(
            "SELECT title, slug, tags, usage, origin, other, summary, aliases FROM memes WHERE instr(title, ?) > 0 ORDER BY length(title) ASC LIMIT 1"
        )
        .bind(q)
        .fetch_optional(pool)
        .await
        {
            return Some(row_to_meme(row));
        }
    }

    None
}

fn row_to_meme(row: sqlx::sqlite::SqliteRow) -> MemeRecord {
    MemeRecord {
        title: row.get("title"),
        slug: row.get("slug"),
        tags: row.get("tags"),
        usage: row.get("usage"),
        origin: row.get("origin"),
        other: row.get("other"),
        summary: row.get("summary"),
        aliases: row.get("aliases"),
    }
}
