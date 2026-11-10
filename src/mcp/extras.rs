//! Extra MCP tools: Bilibili browsing (`bilibili_browse`), QQ profile & avatar updates
//! (`profile_update`), and Qzone posting (`qzone_publish`).

use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;

use crate::mcp::Scope;
use crate::store::SqliteStore;

pub const BILIBILI_TOOL: &str = "bilibili_browse";
pub const PROFILE_TOOL: &str = "profile_update";
pub const QZONE_TOOL: &str = "qzone_publish";
pub const MEMEWIKI_TOOL: &str = "search_meme_wiki";

/// Cooldown for autonomous global profile/avatar change: once per 24 hours.
const SELF_PROFILE_COOLDOWN_SECS: i64 = 86_400;
/// Cooldown for Qzone posting when not explicitly requested by owner: once per 2 hours.
const SELF_QZONE_COOLDOWN_SECS: i64 = 7_200;

/// Tool definitions advertised on `tools/list`.
pub fn tool_definitions() -> Value {
    json!([
        {
            "name": BILIBILI_TOOL,
            "description": "刷B站视频与评论区，并自动从B站评论区收集表情包/梗图补充进你自己的表情库。支持获取B站热门视频榜（popular）、查看指定BV号视频详情与评论区热评（view）、专门深挖评论区神评并收集评论区表情包（comments）、以及搜索B站视频（search）。收集到的表情包会自动入库并返回 [表情 #xxxxxxxx] 句柄，你可以当场放进 submit_reply 的 sticker_ops 发送，或以后随时用 sticker_search 搜索使用。",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "mode": {
                        "type": "string",
                        "enum": ["popular", "view", "comments", "search"],
                        "description": "popular：看B站热门视频榜并顺手逛前排评论区收集表情包；view：看指定BV号视频详情、评论区热评并收集评论区表情包；comments：深挖指定BV号（不填则自动挑热门视频）的评论区神评与楼中楼，大量收集评论区梗图/表情包；search：按关键词搜B站视频"
                    },
                    "bvid": {
                        "type": "string",
                        "description": "mode 为 view 时必填（mode 为 comments 时可选），例如 BV1xx411c7mD"
                    },
                    "keyword": {
                        "type": "string",
                        "description": "mode 为 search 时必填，搜索关键词"
                    },
                    "limit": {
                        "type": "integer",
                        "minimum": 1,
                        "maximum": 10,
                        "description": "返回条数，默认 6"
                    }
                },
                "required": ["mode"]
            }
        },
        {
            "name": PROFILE_TOOL,
            "description": "修改你自己的QQ全局头像、QQ全局昵称、个性签名、本群群名片，或者修改当前QQ群的群名称（group_name）与群聊头像（group_avatar_url）。权限规则：仅当主人（role 为 owner）明确让你改、或者你自己主动想换（非普通群友要求，且自发修改受每日1次冷却限制）时才允许调用；普通群友（role 为 member）让你改头像、改名或改群名/群头像时一律拒绝，不要调用本工具。",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "nickname": {
                        "type": "string",
                        "description": "可选：新的你自己QQ全局昵称（不超过16字）"
                    },
                    "group_card": {
                        "type": "string",
                        "description": "可选：新的你本群群名片（不超过20字）"
                    },
                    "personal_note": {
                        "type": "string",
                        "description": "可选：新的你自己QQ个性签名（不超过50字）"
                    },
                    "avatar_url": {
                        "type": "string",
                        "description": "可选：新的你自己QQ头像，支持图片直链 URL（http/https，如B站封面图）或你表情库里的表情句柄（如 #d6f1cdb9）"
                    },
                    "group_name": {
                        "type": "string",
                        "description": "可选：修改当前QQ群的群聊名称（不超过20字）"
                    },
                    "group_avatar_url": {
                        "type": "string",
                        "description": "可选：修改当前QQ群的群聊头像，支持图片直链 URL（http/https，如B站封面图）或你表情库里的表情句柄（如 #d6f1cdb9）"
                    }
                }
            }
        },
        {
            "name": QZONE_TOOL,
            "description": "发一条你自己的QQ空间说说（动态）。可以写你刚在B站看到的好玩的事、群里的趣事吐槽或日常心情，支持纯文字或附带一张网络图片链接。",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "content": {
                        "type": "string",
                        "description": "说说正文内容（纯文本，符合你的人设语气，不超过500字）"
                    },
                    "image_url": {
                        "type": "string",
                        "description": "可选：配图直链 URL（http/https，如B站封面图）"
                    }
                },
                "required": ["content"]
            }
        },
        {
            "name": MEMEWIKI_TOOL,
            "description": "检索本地离线梗百科词典（包含 6700+ 中文网络流行语、流行梗、黑话出处、缩写与用法）。当你没把握某个词的梗义、或群友询问某个梗的背景与接法时调用。查到后自然融入对话，不要教科书式念经。",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "query": {
                        "type": "string",
                        "description": "要查询的网络梗名称、缩写或关键词（如'去码头整点薯条'、'kskbl'、'二仙桥'）"
                    }
                },
                "required": ["query"]
            }
        }
    ])
}

fn tool_ok(text: impl Into<String>) -> Value {
    json!({
        "content": [{"type": "text", "text": text.into()}],
        "isError": false
    })
}

fn tool_err(text: impl Into<String>) -> Value {
    json!({
        "content": [{"type": "text", "text": text.into()}],
        "isError": true
    })
}

fn truncate_chars(s: &str, max_chars: usize) -> String {
    let mut out: String = s.chars().take(max_chars).collect();
    if s.chars().count() > max_chars {
        out.push('…');
    }
    out
}

fn build_http_client() -> Result<reqwest::Client, String> {
    reqwest::Client::builder()
        .user_agent("Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36")
        .timeout(Duration::from_secs(12))
        .build()
        .map_err(|e| format!("创建 HTTP 客户端失败: {e}"))
}

fn resolve_sticker_dir() -> std::path::PathBuf {
    if let Ok(dir) = std::env::var("STICKER_DIR")
        && !dir.trim().is_empty()
    {
        return std::path::PathBuf::from(dir);
    }
    if let Ok(db) = std::env::var("GEMINI_MEMORY_DB")
        && let Some(parent) = std::path::Path::new(&db).parent()
    {
        return parent.join("stickers");
    }
    std::path::PathBuf::from("data/stickers")
}

fn normalize_bili_img_url(raw: &str) -> String {
    let trimmed = raw.trim();
    if trimmed.starts_with("//") {
        format!("https:{trimmed}")
    } else if let Some(rest) = trimmed.strip_prefix("http://") {
        format!("https://{rest}")
    } else {
        trimmed.to_string()
    }
}

fn strip_bili_emote_brackets(text: &str) -> String {
    let mut out = String::new();
    let mut in_bracket = false;
    for ch in text.chars() {
        match ch {
            '[' => in_bracket = true,
            ']' => in_bracket = false,
            _ if !in_bracket => out.push(ch),
            _ => {}
        }
    }
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn infer_comment_emotion(text: &str) -> String {
    let mut tags = Vec::new();
    if text.contains("笑") || text.contains("哈") || text.contains("草") || text.contains("蚌") || text.contains("乐") {
        tags.push("搞笑");
        tags.push("大笑");
    }
    if text.contains("？”") || text.contains("？") || text.contains("?") || text.contains("懵") || text.contains("啥") {
        tags.push("疑惑");
        tags.push("无语");
    }
    if text.contains("哭") || text.contains("泪") || text.contains("惨") || text.contains("破防") {
        tags.push("委屈");
        tags.push("破防");
    }
    if text.contains("怒") || text.contains("气") || text.contains("急") || text.contains("滚") {
        tags.push("生气");
        tags.push("嫌弃");
    }
    if text.contains("可爱") || text.contains("好耶") || text.contains("喜欢") || text.contains("爱") {
        tags.push("开心");
        tags.push("得意");
    }
    tags.push("吐槽");
    tags.push("玩梗");
    tags.push("吃瓜");
    tags.join("/")
}

struct CommentStickerCandidate {
    img_url: String,
    ocr: String,
    description: String,
    emotion: String,
    is_user_picture: bool,
}

/// Fetch hot comments (plus nested sub-replies) for `aid`, render them into readable lines,
/// and download + ingest comment pictures / emotes into the bot's sticker library (`kept = 1`).
async fn fetch_comments_and_harvest_stickers(
    client: &reqwest::Client,
    store: &SqliteStore,
    scope: &Scope,
    aid: i64,
    video_title: &str,
    max_comments: usize,
    max_new_stickers: usize,
    now: i64,
) -> (Vec<String>, Vec<String>) {
    let mut comment_lines = Vec::new();
    let mut harvested_lines = Vec::new();
    if aid <= 0 {
        return (comment_lines, harvested_lines);
    }

    let reply_url = format!(
        "https://api.bilibili.com/x/v2/reply?type=1&oid={aid}&sort=1&ps={}",
        max_comments.clamp(5, 20)
    );
    let Ok(resp) = client
        .get(&reply_url)
        .header("Referer", "https://www.bilibili.com/")
        .send()
        .await
    else {
        return (comment_lines, harvested_lines);
    };
    let Ok(r_body) = resp.json::<Value>().await else {
        return (comment_lines, harvested_lines);
    };
    let Some(replies) = r_body.pointer("/data/replies").and_then(Value::as_array) else {
        return (comment_lines, harvested_lines);
    };

    let mut candidates: Vec<CommentStickerCandidate> = Vec::new();
    let short_title = truncate_chars(video_title.trim(), 14);

    for (i, rep) in replies.iter().take(max_comments).enumerate() {
        let uname = rep
            .pointer("/member/uname")
            .and_then(Value::as_str)
            .unwrap_or("网友");
        let msg = rep
            .pointer("/content/message")
            .and_then(Value::as_str)
            .unwrap_or("")
            .replace('\n', " ");
        let rlike = rep.get("like").and_then(Value::as_i64).unwrap_or(0);
        let clean_msg = strip_bili_emote_brackets(&msg);

        let mut pic_count = 0usize;
        if let Some(pics) = rep.pointer("/content/pictures").and_then(Value::as_array) {
            for p in pics {
                let Some(src) = p.get("img_src").and_then(Value::as_str) else {
                    continue;
                };
                let w = p.get("img_width").and_then(Value::as_u64).unwrap_or(400);
                let h = p.get("img_height").and_then(Value::as_u64).unwrap_or(400);
                // Skip extreme long screenshots; keep square-ish meme/reaction images
                if w > 0 && h > 0 && (w > h * 3 || h > w * 3) {
                    continue;
                }
                pic_count += 1;
                let ocr_text = if clean_msg.is_empty() {
                    short_title.clone()
                } else {
                    truncate_chars(&clean_msg, 22)
                };
                let desc = format!(
                    "B站评论梗图（{}）配文：{}",
                    short_title,
                    truncate_chars(if clean_msg.is_empty() { "热评配图" } else { &clean_msg }, 22)
                );
                candidates.push(CommentStickerCandidate {
                    img_url: normalize_bili_img_url(src),
                    ocr: ocr_text,
                    description: desc,
                    emotion: infer_comment_emotion(&msg),
                    is_user_picture: true,
                });
            }
        }

        let mut emote_names = Vec::new();
        if let Some(emote_map) = rep.pointer("/content/emote").and_then(Value::as_object) {
            for (k, v) in emote_map {
                let Some(url) = v.get("url").and_then(Value::as_str) else {
                    continue;
                };
                let title = v
                    .get("jump_title")
                    .and_then(Value::as_str)
                    .or_else(|| v.get("text").and_then(Value::as_str))
                    .unwrap_or(k)
                    .trim_matches(|c| c == '[' || c == ']');
                let size = v.pointer("/meta/size").and_then(Value::as_i64).unwrap_or(1);
                emote_names.push(title.to_string());
                candidates.push(CommentStickerCandidate {
                    img_url: normalize_bili_img_url(url),
                    ocr: title.to_string(),
                    description: format!(
                        "B站表情「{}」配评：{}",
                        title,
                        truncate_chars(if clean_msg.is_empty() { video_title } else { &clean_msg }, 20)
                    ),
                    emotion: format!("{title}/{}", infer_comment_emotion(&msg)),
                    // Large Bilibili stickers (size == 2) get same priority as user pictures
                    is_user_picture: size >= 2,
                });
            }
        }

        let mut line = format!(
            "{}. {}（{}赞）：{}",
            i + 1,
            uname,
            rlike,
            truncate_chars(msg.trim(), 100)
        );
        if pic_count > 0 {
            line.push_str(&format!(" [附带{pic_count}张评论梗图]"));
        }
        if !emote_names.is_empty() {
            line.push_str(&format!(" [含B站表情：{}]", emote_names.join("、")));
        }
        comment_lines.push(line);

        // Also scan nested sub-replies for funny replies & pictures
        if let Some(sub_replies) = rep.get("replies").and_then(Value::as_array) {
            for sub in sub_replies.iter().take(2) {
                let sub_uname = sub
                    .pointer("/member/uname")
                    .and_then(Value::as_str)
                    .unwrap_or("网友");
                let sub_msg = sub
                    .pointer("/content/message")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .replace('\n', " ");
                let sub_like = sub.get("like").and_then(Value::as_i64).unwrap_or(0);
                if !sub_msg.trim().is_empty() {
                    comment_lines.push(format!(
                        "   └─ {}（{}赞）：{}",
                        sub_uname,
                        sub_like,
                        truncate_chars(sub_msg.trim(), 80)
                    ));
                }
                if let Some(pics) = sub.pointer("/content/pictures").and_then(Value::as_array) {
                    let clean_sub = strip_bili_emote_brackets(&sub_msg);
                    for p in pics {
                        if let Some(src) = p.get("img_src").and_then(Value::as_str) {
                            candidates.push(CommentStickerCandidate {
                                img_url: normalize_bili_img_url(src),
                                ocr: truncate_chars(if clean_sub.is_empty() { &short_title } else { &clean_sub }, 22),
                                description: format!(
                                    "B站楼中楼梗图（{}）：{}",
                                    short_title,
                                    truncate_chars(&clean_sub, 20)
                                ),
                                emotion: infer_comment_emotion(&sub_msg),
                                is_user_picture: true,
                            });
                        }
                    }
                }
            }
        }
    }

    // Prioritize user-posted comment memes and large Bilibili stickers first
    candidates.sort_by_key(|c| !c.is_user_picture);

    let sticker_dir = resolve_sticker_dir();
    let mut added = 0usize;
    let mut seen_urls = std::collections::HashSet::new();

    for cand in candidates {
        if added >= max_new_stickers {
            break;
        }
        if !seen_urls.insert(cand.img_url.clone()) {
            continue;
        }
        let Ok(img_resp) = client
            .get(&cand.img_url)
            .header("Referer", "https://www.bilibili.com/")
            .send()
            .await
        else {
            continue;
        };
        let Ok(raw_bytes) = img_resp.bytes().await else {
            continue;
        };
        if raw_bytes.is_empty() || raw_bytes.len() > 4 * 1024 * 1024 {
            continue;
        }
        // Validate that the bytes can be encoded as a sticker by OneBot delivery
        if crate::media::collage::fit_for_sticker(&raw_bytes).is_none() {
            continue;
        }
        let hash = crate::memory::stickers::hash_bytes(&raw_bytes);
        let tag = crate::app::preflight::ImageTag {
            index: 1,
            kind: "sticker".to_string(),
            self_ref: "other".to_string(),
            ocr: cand.ocr.clone(),
            description: cand.description.clone(),
            emotion: cand.emotion.clone(),
            nsfw: false,
        };
        let outcome = crate::app::sticker_gate::ingest(
            store,
            &scope.bot_id,
            &sticker_dir,
            crate::app::sticker_gate::TaggedPicture {
                group_id: &scope.group_id,
                message_ref: "bili",
                hash: &hash,
                bytes: &raw_bytes,
                tag: &tag,
                now,
            },
        )
        .await;

        if let Some(resolved_hash) = outcome.hash() {
            let _ = store
                .set_sticker_kept(&scope.bot_id, resolved_hash, crate::store::KEPT_YES)
                .await;
            let short = &resolved_hash[..resolved_hash.len().min(8)];
            harvested_lines.push(format!(
                "- [表情 #{short}] 文字「{}」 | {} | 情绪：{}",
                cand.ocr, cand.description, cand.emotion
            ));
            added += 1;
        }
    }

    (comment_lines, harvested_lines)
}

/// Handle `bilibili_browse`.
pub async fn call_bilibili(store: &SqliteStore, scope: &Scope, args: &Value) -> Value {
    let mode = args
        .get("mode")
        .and_then(Value::as_str)
        .unwrap_or("popular")
        .trim();
    let limit = args
        .get("limit")
        .and_then(Value::as_u64)
        .unwrap_or(6)
        .clamp(1, 10) as usize;

    let client = match build_http_client() {
        Ok(c) => c,
        Err(e) => return tool_err(e),
    };

    let now = crate::now_unix();
    match mode {
        "popular" => {
            let url = format!("https://api.bilibili.com/x/web-interface/popular?ps={limit}&pn=1");
            let resp = match client.get(&url).send().await {
                Ok(r) => r,
                Err(e) => return tool_err(format!("请求B站热门失败: {e}")),
            };
            let body: Value = match resp.json().await {
                Ok(v) => v,
                Err(e) => return tool_err(format!("解析B站热门响应失败: {e}")),
            };
            let list = body
                .pointer("/data/list")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            if list.is_empty() {
                return tool_ok("B站热门列表暂时为空。");
            }
            let mut lines = vec!["【B站当前热门视频】".to_string()];
            let mut all_harvested = Vec::new();
            for (idx, item) in list.iter().take(limit).enumerate() {
                let aid = item.get("aid").and_then(Value::as_i64).unwrap_or(0);
                let bvid = item.get("bvid").and_then(Value::as_str).unwrap_or("");
                let title = item.get("title").and_then(Value::as_str).unwrap_or("");
                let owner = item
                    .pointer("/owner/name")
                    .and_then(Value::as_str)
                    .unwrap_or("未知UP");
                let view = item.pointer("/stat/view").and_then(Value::as_i64).unwrap_or(0);
                let danmaku = item
                    .pointer("/stat/danmaku")
                    .and_then(Value::as_i64)
                    .unwrap_or(0);
                let rcmd = item
                    .pointer("/rcmd_reason/content")
                    .and_then(Value::as_str)
                    .unwrap_or("");
                let pic = item.get("pic").and_then(Value::as_str).unwrap_or("");
                let desc = item
                    .get("desc")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .replace('\n', " ");
                let mut line = format!(
                    "{}. [{}] 《{}》 UP主：{} | 播放：{} 弹幕：{}",
                    idx + 1,
                    bvid,
                    title,
                    owner,
                    view,
                    danmaku
                );
                if !rcmd.is_empty() {
                    line.push_str(&format!(" | 推荐理由：{rcmd}"));
                }
                if !desc.trim().is_empty() {
                    line.push_str(&format!(" | 简介：{}", truncate_chars(desc.trim(), 60)));
                }
                if !pic.is_empty() {
                    line.push_str(&format!(" | 封面：{pic}"));
                }
                lines.push(line);

                // Automatically peek into top 2 popular videos' comment sections and harvest stickers
                if idx < 2 && aid > 0 {
                    let (comments, harvested) = fetch_comments_and_harvest_stickers(
                        &client, store, scope, aid, title, 5, 2, now,
                    )
                    .await;
                    if !comments.is_empty() {
                        lines.push(format!("   [{} 评论区前排神评]：", bvid));
                        for c in comments.into_iter().take(3) {
                            lines.push(format!("   {c}"));
                        }
                    }
                    all_harvested.extend(harvested);
                }
            }
            if !all_harvested.is_empty() {
                lines.push("【顺手从B站评论区收入你表情库的新表情包（可直接用 hash 发送）】".to_string());
                lines.extend(all_harvested);
            }
            lines.push("（想深挖某个视频的评论区或专门搜集更多评论区表情包，可用 mode=\"view\" 或 mode=\"comments\" 并传入 bvid）".to_string());
            let _ = store
                .audit(
                    "agent",
                    &format!("mcp.bilibili.popular.n{}", list.len().min(limit)),
                    Some(&scope.bot_id),
                    Some(&scope.group_id),
                    now,
                )
                .await;
            tool_ok(lines.join("\n"))
        }
        "view" | "comments" => {
            let mut bvid = args
                .get("bvid")
                .and_then(Value::as_str)
                .unwrap_or("")
                .trim()
                .to_string();
            // If mode="comments" and no bvid was supplied, automatically pick a top popular video
            if bvid.is_empty() && mode == "comments" {
                if let Ok(r) = client
                    .get("https://api.bilibili.com/x/web-interface/popular?ps=5&pn=1")
                    .send()
                    .await
                    && let Ok(v) = r.json::<Value>().await
                    && let Some(arr) = v.pointer("/data/list").and_then(Value::as_array)
                {
                    let idx = (now as usize) % arr.len().max(1);
                    if let Some(chosen) = arr.get(idx).and_then(|it| it.get("bvid")).and_then(Value::as_str) {
                        bvid = chosen.to_string();
                    }
                }
            }
            if bvid.is_empty() {
                return tool_err("请提供 bvid（例如 BV1xx...），或使用 mode=\"comments\" 自动巡航热门评论区");
            }
            let view_url = format!("https://api.bilibili.com/x/web-interface/view?bvid={bvid}");
            let resp = match client.get(&view_url).send().await {
                Ok(r) => r,
                Err(e) => return tool_err(format!("请求B站视频详情失败: {e}")),
            };
            let body: Value = match resp.json().await {
                Ok(v) => v,
                Err(e) => return tool_err(format!("解析视频详情失败: {e}")),
            };
            let Some(data) = body.get("data") else {
                return tool_err("未找到该BV号对应的视频");
            };
            let aid = data.get("aid").and_then(Value::as_i64).unwrap_or(0);
            let title = data.get("title").and_then(Value::as_str).unwrap_or("");
            let owner = data
                .pointer("/owner/name")
                .and_then(Value::as_str)
                .unwrap_or("未知UP");
            let desc = data
                .get("desc")
                .and_then(Value::as_str)
                .unwrap_or("")
                .replace('\n', " ");
            let pic = data.get("pic").and_then(Value::as_str).unwrap_or("");
            let view = data.pointer("/stat/view").and_then(Value::as_i64).unwrap_or(0);
            let like = data.pointer("/stat/like").and_then(Value::as_i64).unwrap_or(0);
            let coin = data.pointer("/stat/coin").and_then(Value::as_i64).unwrap_or(0);

            let mut lines = vec![
                format!("【B站视频与评论区】[{bvid}] 《{title}》"),
                format!("UP主：{owner} | 播放：{view} 点赞：{like} 投币：{coin}"),
                format!("简介：{}", truncate_chars(desc.trim(), 200)),
            ];
            if !pic.is_empty() {
                lines.push(format!("封面图URL：{pic}"));
            }

            let (comments, harvested) = fetch_comments_and_harvest_stickers(
                &client, store, scope, aid, title, 12, 5, now,
            )
            .await;
            if !comments.is_empty() {
                lines.push("【评论区高赞神评与楼中楼】".to_string());
                lines.extend(comments);
            }
            let harvested_len = harvested.len();
            if !harvested.is_empty() {
                lines.push("【已从该视频评论区自动收集并存入你表情库的表情包（可在 sticker_ops 中用 send 直接发出）】".to_string());
                lines.extend(harvested);
            }
            let _ = store
                .audit(
                    "agent",
                    &format!("mcp.bilibili.{mode}.{bvid}.stickers{harvested_len}"),
                    Some(&scope.bot_id),
                    Some(&scope.group_id),
                    now,
                )
                .await;
            tool_ok(lines.join("\n"))
        }
        "search" => {
            let keyword = args
                .get("keyword")
                .and_then(Value::as_str)
                .unwrap_or("")
                .trim();
            if keyword.is_empty() {
                return tool_err("mode=search 时必须提供 keyword");
            }
            // Obtain cookies from bilibili.com first so search API doesn't reject with -412
            let _ = client.get("https://www.bilibili.com").send().await;
            let encoded: String = url::form_urlencoded::byte_serialize(keyword.as_bytes()).collect();
            let search_url = format!(
                "https://api.bilibili.com/x/web-interface/search/type?search_type=video&keyword={encoded}&page=1&page_size={limit}"
            );
            let resp = match client
                .get(&search_url)
                .header("Referer", "https://search.bilibili.com/")
                .send()
                .await
            {
                Ok(r) => r,
                Err(e) => return tool_err(format!("搜索B站失败: {e}")),
            };
            let body: Value = match resp.json().await {
                Ok(v) => v,
                Err(e) => return tool_err(format!("解析B站搜索结果失败: {e}")),
            };
            let results = body
                .pointer("/data/result")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            if results.is_empty() {
                return tool_ok(format!("没有搜到关于「{keyword}」的B站视频，可以试试 mode=\"popular\" 看热门榜。"));
            }
            let mut lines = vec![format!("【B站搜索：{keyword}】")];
            let mut first_aid = 0i64;
            let mut first_title = String::new();
            for (idx, item) in results.iter().take(limit).enumerate() {
                let aid = item.get("aid").and_then(Value::as_i64).unwrap_or(0);
                let bvid = item.get("bvid").and_then(Value::as_str).unwrap_or("");
                let raw_title = item.get("title").and_then(Value::as_str).unwrap_or("");
                let title = raw_title
                    .replace("<em class=\"keyword\">", "")
                    .replace("</em>", "");
                if idx == 0 && aid > 0 {
                    first_aid = aid;
                    first_title = title.clone();
                }
                let author = item.get("author").and_then(Value::as_str).unwrap_or("未知UP");
                let play = item.get("play").and_then(Value::as_i64).unwrap_or(0);
                let desc = item
                    .get("description")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .replace('\n', " ");
                let mut pic = item
                    .get("pic")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                if pic.starts_with("//") {
                    pic = format!("https:{pic}");
                }
                lines.push(format!(
                    "{}. [{}] 《{}》 UP主：{} | 播放：{} | 简介：{} | 封面：{}",
                    idx + 1,
                    bvid,
                    title,
                    author,
                    play,
                    truncate_chars(desc.trim(), 50),
                    pic
                ));
            }
            if first_aid > 0 {
                let (comments, harvested) = fetch_comments_and_harvest_stickers(
                    &client,
                    store,
                    scope,
                    first_aid,
                    &first_title,
                    5,
                    3,
                    now,
                )
                .await;
                if !comments.is_empty() {
                    lines.push("【首个搜索结果评论区热评】".to_string());
                    lines.extend(comments.into_iter().take(3));
                }
                if !harvested.is_empty() {
                    lines.push("【从该视频评论区收入你表情库的新表情包】".to_string());
                    lines.extend(harvested);
                }
            }
            let _ = store
                .audit(
                    "agent",
                    "mcp.bilibili.search",
                    Some(&scope.bot_id),
                    Some(&scope.group_id),
                    now,
                )
                .await;
            tool_ok(lines.join("\n"))
        }
        other => tool_err(format!("未知 mode：{other}，请使用 popular / view / comments / search")),
    }
}

/// Call one action on the local OneBot WebSocket server.
async fn onebot_call(action: &str, params: Value) -> Result<Value, String> {
    let ws_url = std::env::var("ONEBOT_WS_URL").unwrap_or_else(|_| "ws://127.0.0.1:3001".into());
    let token = std::env::var("ONEBOT_ACCESS_TOKEN")
        .ok()
        .filter(|s| !s.trim().is_empty());

    let mut request = ws_url
        .as_str()
        .into_client_request()
        .map_err(|e| format!("OneBot URL无效: {e}"))?;
    if let Some(t) = token {
        if let Ok(val) = format!("Bearer {t}").parse() {
            request.headers_mut().insert("Authorization", val);
        }
    }

    let (stream, _) = tokio::time::timeout(
        Duration::from_secs(5),
        tokio_tungstenite::connect_async(request),
    )
    .await
    .map_err(|_| "连接 OneBot 超时".to_string())?
    .map_err(|e| format!("连接 OneBot 失败: {e}"))?;

    let (mut write, mut read) = stream.split();
    let echo = format!("mcp-{}-{}", action, crate::now_unix());
    let frame = json!({
        "action": action,
        "params": params,
        "echo": echo,
    });
    write
        .send(Message::Text(frame.to_string().into()))
        .await
        .map_err(|e| format!("发送 OneBot 指令失败: {e}"))?;

    let deadline = tokio::time::Instant::now() + Duration::from_secs(12);
    while tokio::time::Instant::now() < deadline {
        let next = tokio::time::timeout(Duration::from_secs(5), read.next()).await;
        let Some(Ok(Message::Text(text))) = (match next {
            Ok(v) => v,
            Err(_) => continue,
        }) else {
            continue;
        };
        let Ok(val) = serde_json::from_str::<Value>(&text) else {
            continue;
        };
        if val.get("echo").and_then(Value::as_str) != Some(echo.as_str()) {
            continue;
        }
        let retcode = val.get("retcode").and_then(Value::as_i64).unwrap_or(-1);
        if retcode != 0 {
            let msg = val
                .get("message")
                .or_else(|| val.get("wording"))
                .and_then(Value::as_str)
                .unwrap_or("未知错误");
            return Err(format!("OneBot 拒绝 {action} (retcode={retcode}): {msg}"));
        }
        return Ok(val.get("data").cloned().unwrap_or(Value::Null));
    }
    Err(format!("等待 OneBot {action} 响应超时"))
}

/// Check whether the current turn in `group_id` was triggered by the owner, an autonomous
/// proactive turn, or a normal group member.
async fn check_turn_authority(store: &SqliteStore, scope: &Scope) -> (bool, bool) {
    // Returns (is_owner_present_recently, is_proactive_turn)
    let is_proactive = match store.armed_turn(&scope.group_id).await {
        Ok(Some(arm)) => arm.batch_id.contains("proactive"),
        _ => false,
    };
    let recent = store
        .recent_group_messages(&scope.bot_id, &scope.group_id, 5)
        .await
        .unwrap_or_default();
    // Look at the most recent non-bot messages in the last 120 seconds
    let now = crate::now_unix();
    let owner_spoke_recently = recent
        .iter()
        .rev()
        .filter(|m| m.speaker_role != "bot" && (now - m.created_at).abs() <= 180)
        .take(3)
        .any(|m| m.speaker_role == "owner");
    (owner_spoke_recently, is_proactive)
}

/// Count how many audit rows matching `action_prefix` exist since `since_ts`.
async fn count_audit_since(store: &SqliteStore, action_prefix: &str, since_ts: i64) -> i64 {
    let pattern = format!("{action_prefix}%");
    sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*) FROM audit_events WHERE action LIKE ? AND created_at >= ?",
    )
    .bind(pattern)
    .bind(since_ts)
    .fetch_one(store.pool())
    .await
    .unwrap_or(0)
}

/// Resolve an image reference (`http(s)://...`, `base64://...`, or a sticker handle like `#d6f1cdb9`)
/// into a `file` value accepted by OneBot `set_qq_avatar` / `set_group_portrait`.
async fn resolve_avatar_source(
    store: &SqliteStore,
    bot_id: &str,
    raw: &str,
) -> Result<String, String> {
    let trimmed = raw.trim();
    if trimmed.starts_with("http://")
        || trimmed.starts_with("https://")
        || trimmed.starts_with("base64://")
    {
        return Ok(trimmed.to_string());
    }
    let handle = trimmed
        .trim_start_matches("表情")
        .trim()
        .trim_start_matches('#')
        .to_lowercase();
    if handle.is_empty() {
        return Err("头像参数不能为空".to_string());
    }
    let full = match store.sticker_by_hash(bot_id, &handle).await {
        Ok(Some(row)) => row.hash,
        Ok(None) => match store.find_sticker_by_prefix(bot_id, &handle).await {
            Ok(Some(h)) => h,
            _ => {
                return Err(
                    "头像参数必须是 http(s):// 图片直链，或表情库中已有的 #xxxxxxxx 句柄".to_string(),
                )
            }
        },
        Err(_) => return Err("读取表情库失败".to_string()),
    };
    let path = resolve_sticker_dir().join(format!("{full}.jpg"));
    let bytes = tokio::fs::read(&path)
        .await
        .map_err(|_| format!("未找到表情图片文件: #{handle}"))?;
    let jpeg = crate::media::collage::fit_for_sticker(&bytes).unwrap_or(bytes);
    Ok(format!(
        "base64://{}",
        crate::media::collage::base64_encode(&jpeg)
    ))
}

/// Handle `profile_update`.
pub async fn call_profile_update(store: &SqliteStore, scope: &Scope, args: &Value) -> Value {
    let nickname = args
        .get("nickname")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let group_card = args
        .get("group_card")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let personal_note = args
        .get("personal_note")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let avatar_url = args
        .get("avatar_url")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let group_name = args
        .get("group_name")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let group_avatar_url = args
        .get("group_avatar_url")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty());

    if nickname.is_none()
        && group_card.is_none()
        && personal_note.is_none()
        && avatar_url.is_none()
        && group_name.is_none()
        && group_avatar_url.is_none()
    {
        return tool_err(
            "请至少提供 nickname、group_card、personal_note、avatar_url、group_name 或 group_avatar_url 之一",
        );
    }

    let now = crate::now_unix();
    let (owner_recent, is_proactive) = check_turn_authority(store, scope).await;

    // Cooldown check if not triggered by owner
    if !owner_recent {
        let used = count_audit_since(
            store,
            "mcp.profile.self_update",
            now - SELF_PROFILE_COOLDOWN_SECS,
        )
        .await;
        if used > 0 {
            return tool_err(
                "你今天已经自己换过一次资料/头像/群设置啦（自发更换每天限1次），除非主人亲自发话让你改。",
            );
        }
    }

    let mut done = Vec::new();
    let mut errors = Vec::new();

    if let Some(nick) = nickname {
        if nick.chars().count() > 16 {
            return tool_err("nickname 不能超过 16 个字符");
        }
        let mut params = json!({ "nickname": nick });
        if let Some(note) = personal_note {
            params["personal_note"] = json!(note);
        }
        match onebot_call("set_qq_profile", params).await {
            Ok(_) => done.push(format!("QQ全局昵称已改为「{nick}」")),
            Err(e) => errors.push(format!("改QQ昵称失败: {e}")),
        }
    } else if let Some(note) = personal_note {
        match onebot_call("set_self_longnick", json!({ "longNick": note })).await {
            Ok(_) => done.push(format!("个性签名已改为「{note}」")),
            Err(e) => errors.push(format!("改个性签名失败: {e}")),
        }
    }

    if let Some(card) = group_card {
        if card.chars().count() > 20 {
            return tool_err("group_card 不能超过 20 个字符");
        }
        match onebot_call(
            "set_group_card",
            json!({
                "group_id": scope.group_id,
                "user_id": scope.bot_id,
                "card": card
            }),
        )
        .await
        {
            Ok(_) => done.push(format!("本群名片已改为「{card}」")),
            Err(e) => errors.push(format!("改群名片失败: {e}")),
        }
    }

    if let Some(url) = avatar_url {
        match resolve_avatar_source(store, &scope.bot_id, url).await {
            Ok(file_val) => match onebot_call("set_qq_avatar", json!({ "file": file_val })).await {
                Ok(_) => done.push("QQ全局头像已更新".to_string()),
                Err(e) => errors.push(format!("改QQ头像失败: {e}")),
            },
            Err(e) => errors.push(e),
        }
    }

    if let Some(gname) = group_name {
        if gname.chars().count() > 20 {
            return tool_err("group_name 不能超过 20 个字符");
        }
        match onebot_call(
            "set_group_name",
            json!({
                "group_id": scope.group_id,
                "group_name": gname
            }),
        )
        .await
        {
            Ok(_) => {
                let _ = store
                    .set_group_info(&scope.bot_id, &scope.group_id, Some(gname), None, now)
                    .await;
                done.push(format!("本群群名称已改为「{gname}」"));
            }
            Err(e) => errors.push(format!("改群名称失败（请确认机器人是否已设为群管理员）: {e}")),
        }
    }

    if let Some(gurl) = group_avatar_url {
        match resolve_avatar_source(store, &scope.bot_id, gurl).await {
            Ok(file_val) => match onebot_call(
                "set_group_portrait",
                json!({
                    "group_id": scope.group_id,
                    "file": file_val
                }),
            )
            .await
            {
                Ok(_) => done.push("本群群聊头像已更新".to_string()),
                Err(e) => errors.push(format!("改群头像失败（请确认机器人是否已设为群管理员）: {e}")),
            },
            Err(e) => errors.push(e),
        }
    }

    if !done.is_empty() {
        let tag = if owner_recent {
            format!("mcp.profile.owner_update.g{}", scope.group_id)
        } else if is_proactive {
            format!("mcp.profile.self_update.proactive.g{}", scope.group_id)
        } else {
            format!("mcp.profile.self_update.g{}", scope.group_id)
        };
        let _ = store
            .audit("agent", &tag, Some(&scope.bot_id), Some(&scope.group_id), now)
            .await;
    }

    if errors.is_empty() {
        tool_ok(done.join("；"))
    } else if !done.is_empty() {
        tool_ok(format!("部分成功：{}（失败：{}）", done.join("；"), errors.join("；")))
    } else {
        tool_err(errors.join("；"))
    }
}

/// Handle `qzone_publish`.
pub async fn call_qzone_publish(store: &SqliteStore, scope: &Scope, args: &Value) -> Value {
    let content = args
        .get("content")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim();
    let image_url = args
        .get("image_url")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty());

    if content.is_empty() && image_url.is_none() {
        return tool_err("说说内容 content 不能为空");
    }
    if content.chars().count() > 500 {
        return tool_err("说说内容不能超过 500 字");
    }

    let now = crate::now_unix();
    let (owner_recent, _) = check_turn_authority(store, scope).await;
    if !owner_recent {
        let used =
            count_audit_since(store, "mcp.qzone.publish", now - SELF_QZONE_COOLDOWN_SECS).await;
        if used > 0 {
            return tool_err("你过去2小时内刚发过QQ空间说说，休息一下晚点再发吧。");
        }
    }

    let images: Vec<String> = match image_url {
        Some(u) if u.starts_with("http://") || u.starts_with("https://") => vec![u.to_string()],
        Some(_) => return tool_err("image_url 必须是 http:// 或 https:// 开头的图片直链"),
        None => Vec::new(),
    };

    let params = json!({
        "content": content,
        "images": images,
        "ugc_right": 1
    });

    match onebot_call("send_qzone_msg", params).await {
        Ok(data) => {
            let tid = data.get("tid").and_then(Value::as_str).unwrap_or("unknown");
            let _ = store
                .audit(
                    "agent",
                    &format!("mcp.qzone.publish.ok.tid_{}", truncate_chars(tid, 16)),
                    Some(&scope.bot_id),
                    Some(&scope.group_id),
                    now,
                )
                .await;
            tool_ok(format!("QQ空间说说发表成功（tid: {tid}）"))
        }
        Err(e) => {
            let _ = store
                .audit(
                    "agent",
                    "mcp.qzone.publish.err",
                    Some(&scope.bot_id),
                    Some(&scope.group_id),
                    now,
                )
                .await;
            tool_err(format!("发表QQ空间说说失败: {e}"))
        }
    }
}

pub async fn call_search_meme_wiki(
    store: &SqliteStore,
    _scope: &Scope,
    args: &Value,
) -> Value {
    let query = match args.get("query").and_then(Value::as_str) {
        Some(q) if !q.trim().is_empty() => q.trim(),
        _ => return tool_err("缺少 query 参数或 query 为空"),
    };

    match crate::memory::memewiki::search_meme(&store.pool, query).await {
        Some(record) => {
            let mut text = format!("【查询命中】：{}\n", record.title);
            if let Some(aliases) = &record.aliases {
                if !aliases.trim().is_empty() {
                    text.push_str(&format!("别名/缩写：{}\n", aliases.trim()));
                }
            }
            if let Some(summary) = &record.summary {
                if !summary.trim().is_empty() {
                    text.push_str(&format!("释义：{}\n", summary.trim()));
                }
            }
            if let Some(origin) = &record.origin {
                if !origin.trim().is_empty() {
                    text.push_str(&format!("梗出处：{}\n", origin.trim()));
                }
            }
            if let Some(usage) = &record.usage {
                if !usage.trim().is_empty() {
                    text.push_str(&format!("用法/接法：{}\n", usage.trim()));
                }
            }
            tool_ok(text)
        }
        None => tool_ok(format!("梗百科知识库中暂未收录「{query}」相关词条。可以如实回答不知道，或结合上下文自然接话，切勿胡编乱造。")),
    }
}
