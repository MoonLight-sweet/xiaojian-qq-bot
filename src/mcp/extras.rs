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
pub const LOCAL_PC_TOOL: &str = "local_pc_control";
pub const STUDY_HUB_TOOL: &str = "study_hub";

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
        },
        {
            "name": LOCAL_PC_TOOL,
            "description": "全方位连接并操控主人挂载的本地Windows电脑外接节点（支持启动任意桌面应用与游戏、强制切换窗口到最前台、关闭窗口、截屏看图、鼠标点击/拖拽/滚轮、键盘按键/组合快捷键、输入任意中英文文字、执行本地PowerShell命令，以及《杀戮尖塔2》专属实时存档/日志读取、单回合批量出牌与死因复盘进化系统）。每次执行动作后不仅可把实时画面发到QQ群，还会把最新屏幕画面保存为当前工作区下的 latest_screen.jpg（1280x800坐标系，红十字标出当前鼠标位置），你可以直接调用 read_file 读取 latest_screen.jpg 看清画面上的按钮位置再连续操作（工作区下的 sts2_knowledge/sts2_strategy_guide.md 还存有B站与小黑盒《杀戮尖塔2》完整攻略）。权限与隐私铁律：仅限主人（role 为 owner）明确指示时才允许调用本工具进行截屏或操作电脑；普通群友要求时一律温和婉拒。",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "mode": {
                        "type": "string",
                        "enum": [
                            "status",
                            "open_app",
                            "focus_window",
                            "close_window",
                            "screenshot",
                            "click",
                            "drag",
                            "scroll",
                            "key",
                            "type_text",
                            "run_command",
                            "sts2_status",
                            "sts2_batch",
                            "sts2_review",
                            "sts2_autoplay_start",
                            "sts2_autoplay_stop"
                        ],
                        "description": "status：查看当前已打开窗口与本机已安装应用/游戏列表；open_app：按名称启动本地应用、游戏（如'杀戮尖塔2'、'微信'、'星露谷'、'星铁'、'B站'、'网易云音乐'、'Steam'、'Chrome'）或网址；focus_window：强制将指定窗口切到最前台；close_window：关闭指定窗口；screenshot：截取实时屏幕；click：鼠标点击(x,y)；drag：从(x,y)拖拽到(to_x,to_y)；scroll：滚动滚轮；key：按键或组合键(如 alt+tab, win+d, space, enter, esc)；type_text：输入中英文文字；run_command：执行本地PowerShell命令；sts2_status：读取《杀戮尖塔2》2号练习槽位的实时存档(含官方中文卡牌/遗物效果)、最新战斗日志、流派便签与死因避坑铁律并截图；sts2_batch：单回合一次性批量执行多步《杀戮尖塔2》按键/点击/拖拽动作并在动画结束后返回最新截图与日志；sts2_review：更新《杀戮尖塔2》局内流派规划便签，或在阵亡/通关后写入死因复盘与怪物避坑铁律；sts2_autoplay_start：开启后台全自动连续爬塔循环（无需主人催促自动一回合接一回合连续推进直到通关或阵亡复盘）；sts2_autoplay_stop：立即停止后台自动爬塔循环"
                    },
                    "app_name": {
                        "type": "string",
                        "description": "mode 为 open_app 时要启动的应用/游戏名称、路径或网址（如'杀戮尖塔'、'微信'、'哔哩哔哩'、'Steam'、'notepad'）"
                    },
                    "window_title": {
                        "type": "string",
                        "description": "mode 为 focus_window 或 close_window 时的窗口标题关键词"
                    },
                    "send_to_group": {
                        "type": "boolean",
                        "description": "是否把操作后的本地电脑实时画面截图直接发送到当前QQ群（默认 true）"
                    },
                    "x": {
                        "type": "integer",
                        "description": "mode 为 click / drag / scroll 时的横向坐标（可直接按 latest_screen.jpg 的 1280x800 坐标系填写，系统会自动换算到 2560x1600 物理屏幕）"
                    },
                    "y": {
                        "type": "integer",
                        "description": "mode 为 click / drag / scroll 时的纵向坐标（基于 1280x800 坐标系）"
                    },
                    "to_x": {
                        "type": "integer",
                        "description": "mode 为 drag 时的拖拽终点横向坐标"
                    },
                    "to_y": {
                        "type": "integer",
                        "description": "mode 为 drag 时的拖拽终点纵向坐标"
                    },
                    "scroll_amount": {
                        "type": "integer",
                        "description": "mode 为 scroll 时的滚轮步数（正数向上滚，负数向下滚，默认 -3）"
                    },
                    "button": {
                        "type": "string",
                        "enum": ["left", "right", "double"],
                        "description": "鼠标按键，默认 left"
                    },
                    "key": {
                        "type": "string",
                        "description": "mode 为 key 时按下的按键或组合键（如 space, enter, esc, up, down, left, right, alt+tab, win+d, ctrl+w, ctrl+v）"
                    },
                    "text": {
                        "type": "string",
                        "description": "mode 为 type_text 时要输入的中英文文本内容"
                    },
                    "press_enter": {
                        "type": "boolean",
                        "description": "mode 为 type_text 时输入完毕后是否自动按下回车键（默认 false）"
                    },
                    "command": {
                        "type": "string",
                        "description": "mode 为 run_command 时要在本机执行的 PowerShell 命令"
                    },
                    "steps": {
                        "type": "array",
                        "description": "mode 为 sts2_batch 时的批量动作列表，例如 [{\"type\":\"key\",\"key\":\"1\"},{\"type\":\"click\",\"x\":900,\"y\":420},{\"type\":\"key\",\"key\":\"e\"}]。支持 type: key / click / drag / wait",
                        "items": {
                            "type": "object"
                        }
                    },
                    "wait_after_ms": {
                        "type": "integer",
                        "description": "mode 为 sts2_batch 时执行完所有动作后等待动画结束再截图的毫秒数（默认 2200）"
                    },
                    "character": {
                        "type": "string",
                        "description": "mode 为 sts2_review 时记录的当前《杀戮尖塔2》角色名称"
                    },
                    "target_archetype": {
                        "type": "string",
                        "description": "mode 为 sts2_review 时更新的本局目标流派方向（如'储君-锻造王刃星能流'、'亡灵契约师-灾厄处决流'、'铁甲战士-烧牌小循环流'）"
                    },
                    "plan_note": {
                        "type": "string",
                        "description": "mode 为 sts2_review 时更新的当前层抓牌/路线规划便签备注"
                    },
                    "enemy_name": {
                        "type": "string",
                        "description": "mode 为 sts2_review 时复盘的怪物/首领名称（如'旧日雕像'、'骇鳗'、'蜂群术士'）"
                    },
                    "death_cause": {
                        "type": "string",
                        "description": "mode 为 sts2_review 时记录的阵亡/翻车死因诊断"
                    },
                    "iron_rule": {
                        "type": "string",
                        "description": "mode 为 sts2_review 时提炼的下次遇到该怪物的具体避坑铁律"
                    },
                    "run_summary": {
                        "type": "string",
                        "description": "mode 为 sts2_review 时记录的整局复盘总结"
                    }
                },
                "required": ["mode"]
            }
        },
        {
            "name": STUDY_HUB_TOOL,
            "description": "个性化学习计划与在线互动测验中心（仿 Gemini 的『查看你的个性化学习计划』与『通过测验检验学习成果』）。调用 create_plan 或 create_quiz 后会自动渲染一张高清精美预览卡片图并直接发送到当前 QQ 群，同时返回一条专属可点击的互动网页链接（用户点开即可像 Gemini 一样在线勾选学习进度、点选 A/B/C/D 选项实时判分看解析，进度与错题会自动回传保存）。你只需在群回复里把返回的链接自然地发给对方即可。",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "mode": {
                        "type": "string",
                        "enum": ["create_plan", "get_plan", "create_quiz", "get_quiz_result"],
                        "description": "create_plan：制定新的个性化学习计划并自动发卡片预览图到群里；get_plan：查看当前最新的学习计划进度（用户在网页上勾选后会实时同步）并重发最新进度图；create_quiz：出一套3-10道题的在线互动测验并自动发测验预览图到群里；get_quiz_result：查看用户最近一次测验的得分、作答进度与错题明细以便点评讲解"
                    },
                    "subject": {
                        "type": "string",
                        "description": "学习主题或学科标签（如 'SpringBoot 后端'、'英语六级'、'数据结构'）"
                    },
                    "title": {
                        "type": "string",
                        "description": "学习计划或测验的主标题（简洁醒目，如 'SpringBoot 7天实战突破计划'）"
                    },
                    "summary": {
                        "type": "string",
                        "description": "mode 为 create_plan 时的一句话总体目标与节奏简介"
                    },
                    "difficulty": {
                        "type": "string",
                        "description": "mode 为 create_quiz 时的难度标签（如 '入门'、'适中'、'进阶'）"
                    },
                    "stages": {
                        "type": "array",
                        "description": "mode 为 create_plan 时的分阶段任务列表，每项含 stage_title（阶段名）与 tasks（任务数组，每项含 text 与可选 tip 学习要点）",
                        "items": {
                            "type": "object",
                            "properties": {
                                "stage_title": {"type": "string"},
                                "tasks": {
                                    "type": "array",
                                    "items": {
                                        "type": "object",
                                        "properties": {
                                            "text": {"type": "string"},
                                            "tip": {"type": "string"},
                                            "done": {"type": "boolean"}
                                        },
                                        "required": ["text"]
                                    }
                                }
                            },
                            "required": ["stage_title", "tasks"]
                        }
                    },
                    "questions": {
                        "type": "array",
                        "description": "mode 为 create_quiz 时的选择题数组（建议3到6题），每题含 question（题干）、options（4个选项字符串数组）、answer（正确答案索引0-3）、explanation（详细解析）、hint（可选解题提示）、tag（可选知识点标签）",
                        "items": {
                            "type": "object",
                            "properties": {
                                "question": {"type": "string"},
                                "options": {"type": "array", "items": {"type": "string"}},
                                "answer": {"type": "integer", "minimum": 0, "maximum": 3},
                                "explanation": {"type": "string"},
                                "hint": {"type": "string"},
                                "tag": {"type": "string"}
                            },
                            "required": ["question", "options", "answer", "explanation"]
                        }
                    },
                    "send_to_group": {
                        "type": "boolean",
                        "description": "是否自动把生成的高清卡片预览图直接发送到当前QQ群（默认 true）"
                    }
                },
                "required": ["mode"]
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
    if text.contains("笑")
        || text.contains("哈")
        || text.contains("草")
        || text.contains("蚌")
        || text.contains("乐")
    {
        tags.push("搞笑");
        tags.push("大笑");
    }
    if text.contains("？”")
        || text.contains("？")
        || text.contains("?")
        || text.contains("懵")
        || text.contains("啥")
    {
        tags.push("疑惑");
        tags.push("无语");
    }
    if text.contains("哭") || text.contains("泪") || text.contains("惨") || text.contains("破防")
    {
        tags.push("委屈");
        tags.push("破防");
    }
    if text.contains("怒") || text.contains("气") || text.contains("急") || text.contains("滚")
    {
        tags.push("生气");
        tags.push("嫌弃");
    }
    if text.contains("可爱")
        || text.contains("好耶")
        || text.contains("喜欢")
        || text.contains("爱")
    {
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
                    truncate_chars(
                        if clean_msg.is_empty() {
                            "热评配图"
                        } else {
                            &clean_msg
                        },
                        22
                    )
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
                        truncate_chars(
                            if clean_msg.is_empty() {
                                video_title
                            } else {
                                &clean_msg
                            },
                            20
                        )
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
                                ocr: truncate_chars(
                                    if clean_sub.is_empty() {
                                        &short_title
                                    } else {
                                        &clean_sub
                                    },
                                    22,
                                ),
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
                let view = item
                    .pointer("/stat/view")
                    .and_then(Value::as_i64)
                    .unwrap_or(0);
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
                lines.push(
                    "【顺手从B站评论区收入你表情库的新表情包（可直接用 hash 发送）】".to_string(),
                );
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
                    if let Some(chosen) = arr
                        .get(idx)
                        .and_then(|it| it.get("bvid"))
                        .and_then(Value::as_str)
                    {
                        bvid = chosen.to_string();
                    }
                }
            }
            if bvid.is_empty() {
                return tool_err(
                    "请提供 bvid（例如 BV1xx...），或使用 mode=\"comments\" 自动巡航热门评论区",
                );
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
            let view = data
                .pointer("/stat/view")
                .and_then(Value::as_i64)
                .unwrap_or(0);
            let like = data
                .pointer("/stat/like")
                .and_then(Value::as_i64)
                .unwrap_or(0);
            let coin = data
                .pointer("/stat/coin")
                .and_then(Value::as_i64)
                .unwrap_or(0);

            let mut lines = vec![
                format!("【B站视频与评论区】[{bvid}] 《{title}》"),
                format!("UP主：{owner} | 播放：{view} 点赞：{like} 投币：{coin}"),
                format!("简介：{}", truncate_chars(desc.trim(), 200)),
            ];
            if !pic.is_empty() {
                lines.push(format!("封面图URL：{pic}"));
            }

            let (comments, harvested) =
                fetch_comments_and_harvest_stickers(&client, store, scope, aid, title, 12, 5, now)
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
            let encoded: String =
                url::form_urlencoded::byte_serialize(keyword.as_bytes()).collect();
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
                return tool_ok(format!(
                    "没有搜到关于「{keyword}」的B站视频，可以试试 mode=\"popular\" 看热门榜。"
                ));
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
                let author = item
                    .get("author")
                    .and_then(Value::as_str)
                    .unwrap_or("未知UP");
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
        other => tool_err(format!(
            "未知 mode：{other}，请使用 popular / view / comments / search"
        )),
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
                    "头像参数必须是 http(s):// 图片直链，或表情库中已有的 #xxxxxxxx 句柄"
                        .to_string(),
                );
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
            Err(e) => errors.push(format!(
                "改群名称失败（请确认机器人是否已设为群管理员）: {e}"
            )),
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
                Err(e) => errors.push(format!(
                    "改群头像失败（请确认机器人是否已设为群管理员）: {e}"
                )),
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
            .audit(
                "agent",
                &tag,
                Some(&scope.bot_id),
                Some(&scope.group_id),
                now,
            )
            .await;
    }

    if errors.is_empty() {
        tool_ok(done.join("；"))
    } else if !done.is_empty() {
        tool_ok(format!(
            "部分成功：{}（失败：{}）",
            done.join("；"),
            errors.join("；")
        ))
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

pub async fn call_search_meme_wiki(store: &SqliteStore, _scope: &Scope, args: &Value) -> Value {
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
        None => tool_ok(format!(
            "梗百科知识库中暂未收录「{query}」相关词条。可以如实回答不知道，或结合上下文自然接话，切勿胡编乱造。"
        )),
    }
}

pub async fn call_local_pc_control(store: &SqliteStore, scope: &Scope, args: &Value) -> Value {
    let mode = args
        .get("mode")
        .and_then(Value::as_str)
        .unwrap_or("status")
        .trim();
    let send_to_group = args
        .get("send_to_group")
        .and_then(Value::as_bool)
        .unwrap_or(mode != "status");

    if mode != "status" {
        let (is_owner, _) = check_turn_authority(store, scope).await;
        if !is_owner {
            return tool_err(
                "隐私与权限保护：仅主人（owner）本人指令才允许对主人的本地电脑执行截屏、启动应用或键鼠窗口操作，普通群友无权查看或操作主人桌面。",
            );
        }
    }

    let bridge_url =
        std::env::var("LOCAL_PC_BRIDGE_URL").unwrap_or_else(|_| "http://127.0.0.1:18095".into());
    let client = match reqwest::Client::builder()
        .timeout(Duration::from_secs(20))
        .build()
    {
        Ok(c) => c,
        Err(e) => return tool_err(format!("创建 HTTP 客户端失败: {e}")),
    };

    let resp = match client
        .post(format!("{}/api", bridge_url.trim_end_matches('/')))
        .header("X-Bridge-Token", "xiaojian-local-pc-bridge-2026")
        .json(args)
        .send()
        .await
    {
        Ok(r) => r,
        Err(_) => {
            return tool_ok(
                "主人的本地Windows电脑当前未挂载在线（或已关机断开隧道），小简目前运行在纯云端常驻模式。",
            );
        }
    };

    let body: Value = match resp.json().await {
        Ok(v) => v,
        Err(e) => return tool_err(format!("解析本地电脑桥接响应失败: {e}")),
    };

    if body.get("ok").and_then(Value::as_bool) != Some(true) {
        let err_msg = body
            .get("error")
            .and_then(Value::as_str)
            .unwrap_or("未知错误");
        return tool_err(format!("本地电脑执行失败: {err_msg}"));
    }

    let active_win = body
        .get("active_window")
        .and_then(Value::as_str)
        .unwrap_or("未知窗口");
    let screen_size = body
        .get("screen_size")
        .and_then(Value::as_str)
        .unwrap_or("未知");
    let action_note = body
        .get("action_note")
        .and_then(Value::as_str)
        .unwrap_or("状态查询完成");
    let windows = body
        .get("windows")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(Value::as_str)
                .take(15)
                .collect::<Vec<_>>()
                .join("、")
        })
        .unwrap_or_default();
    let installed_apps = body
        .get("installed_apps")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(Value::as_str)
                .take(45)
                .collect::<Vec<_>>()
                .join("、")
        })
        .unwrap_or_default();

    let mut sent_img_note = String::new();
    let mut saved_file_note = String::new();
    if let Some(b64) = body.get("image_base64").and_then(Value::as_str)
        && !b64.is_empty()
    {
        // 保存一份到当前群的常驻工作区，供模型自己用 read_file 看清画面和坐标
        if let Ok(bytes) = base64::Engine::decode(&base64::engine::general_purpose::STANDARD, b64) {
            let projects_dir = std::env::var("AGY_PROJECTS_DIR")
                .unwrap_or_else(|_| "/opt/gemini-qq-bot/data/agy-projects".into());
            let screen_path = std::path::Path::new(&projects_dir)
                .join(&scope.group_id)
                .join("latest_screen.jpg");
            if std::fs::write(&screen_path, &bytes).is_ok() {
                saved_file_note = "\n- 视觉画面文件：已保存至当前工作区 `latest_screen.jpg`（尺寸 1280x800，红十字为当前鼠标位置；如需看清界面按钮位置以进行下一步点击，可直接调用 read_file 查看 `latest_screen.jpg`）".to_string();
            }
        }
        if send_to_group && let Ok(gid) = scope.group_id.parse::<i64>() {
            let params = json!({
                "group_id": gid,
                "message": [
                    {
                        "type": "image",
                        "data": {
                            "file": format!("base64://{b64}")
                        }
                    }
                ]
            });
            match onebot_call("send_group_msg", params).await {
                Ok(_) => {
                    sent_img_note = "（已将本地电脑实时画面截图直接发送至当前QQ群）".to_string();
                }
                Err(e) => {
                    sent_img_note = format!("（截图发群失败: {e}）");
                }
            }
        }
    }

    let _ = store
        .audit(
            "agent",
            &format!("mcp.local_pc.{mode}"),
            Some(&scope.bot_id),
            Some(&scope.group_id),
            crate::now_unix(),
        )
        .await;

    let mut report = format!(
        "【本地Windows电脑节点已连接】\n- 执行结果：{action_note}{sent_img_note}\n- 当前前台窗口：{active_win}\n- 屏幕分辨率：{screen_size}\n- 当前已打开窗口：{windows}{saved_file_note}"
    );
    if !installed_apps.is_empty() {
        report.push_str(&format!(
            "\n- 本机可直接启动的应用与游戏（mode=\"open_app\"）：{installed_apps}"
        ));
    }
    tool_ok(report)
}

pub async fn call_study_hub(store: &SqliteStore, scope: &Scope, args: &Value) -> Value {
    let mode = args
        .get("mode")
        .and_then(Value::as_str)
        .unwrap_or("get_plan");
    let send_to_group = args
        .get("send_to_group")
        .and_then(Value::as_bool)
        .unwrap_or(true);

    let endpoint = match mode {
        "create_plan" => "/internal/api/plan/create",
        "get_plan" => "/internal/api/plan/latest",
        "create_quiz" => "/internal/api/quiz/create",
        "get_quiz_result" => "/internal/api/quiz/latest",
        other => return tool_err(format!("不支持的 study_hub mode: {other}")),
    };

    let mut payload = args.clone();
    if let Some(obj) = payload.as_object_mut() {
        obj.insert("group_id".into(), json!(scope.group_id));
    }

    let client = match reqwest::Client::builder()
        .timeout(Duration::from_secs(20))
        .build()
    {
        Ok(c) => c,
        Err(e) => return tool_err(format!("创建 HTTP 客户端失败: {e}")),
    };

    let base_url =
        std::env::var("STUDY_HUB_INTERNAL_URL").unwrap_or_else(|_| "http://127.0.0.1:18086".into());
    let resp = match client
        .post(format!("{}{}", base_url.trim_end_matches('/'), endpoint))
        .header("X-Study-Token", "xiaojian-study-hub-2026")
        .json(&payload)
        .send()
        .await
    {
        Ok(r) => r,
        Err(e) => return tool_err(format!("学习测验中心服务未响应: {e}")),
    };

    let body: Value = match resp.json().await {
        Ok(v) => v,
        Err(e) => return tool_err(format!("解析学习测验中心响应失败: {e}")),
    };

    if body.get("ok").and_then(Value::as_bool) != Some(true) {
        let err_msg = body
            .get("error")
            .and_then(Value::as_str)
            .unwrap_or("未知错误");
        return tool_err(format!("学习测验中心执行失败: {err_msg}"));
    }

    let mut img_note = String::new();
    if send_to_group
        && let Some(b64) = body.get("image_base64").and_then(Value::as_str)
        && !b64.is_empty()
        && let Ok(gid) = scope.group_id.parse::<i64>()
    {
        let params = json!({
            "group_id": gid,
            "message": [
                {
                    "type": "image",
                    "data": {
                        "file": format!("base64://{b64}")
                    }
                }
            ]
        });
        match onebot_call("send_group_msg", params).await {
            Ok(_) => img_note = "（已将高清卡片预览图直接发送到当前QQ群）".to_string(),
            Err(e) => img_note = format!("（卡片预览图发群失败: {e}）"),
        }
    }

    let _ = store
        .audit(
            "agent",
            &format!("mcp.study_hub.{mode}"),
            Some(&scope.bot_id),
            Some(&scope.group_id),
            crate::now_unix(),
        )
        .await;

    let url = body.get("url").and_then(Value::as_str).unwrap_or("");
    let local_url = body.get("local_url").and_then(Value::as_str).unwrap_or("");

    match mode {
        "create_plan" => {
            let done = body.get("done").and_then(Value::as_i64).unwrap_or(0);
            let total = body.get("total").and_then(Value::as_i64).unwrap_or(0);
            let pct = body.get("pct").and_then(Value::as_i64).unwrap_or(0);
            tool_ok(format!(
                "个性化学习计划已生成{img_note}\n- 当前进度：{done}/{total}（{pct}%）\n- 专属互动链接：{url}\n- 本机隧道直连备用链接：{local_url}\n请在回复中把链接直接发给对方，提醒对方点开即可在线勾选任务进度和查看学习要点。"
            ))
        }
        "get_plan" => {
            if body.get("found").and_then(Value::as_bool) != Some(true) {
                return tool_ok(
                    "当前还没有创建过学习计划，可以直接调用 mode=\"create_plan\" 为对方量身定制一份。",
                );
            }
            let title = body.get("title").and_then(Value::as_str).unwrap_or("");
            let done = body.get("done").and_then(Value::as_i64).unwrap_or(0);
            let total = body.get("total").and_then(Value::as_i64).unwrap_or(0);
            let pct = body.get("pct").and_then(Value::as_i64).unwrap_or(0);
            let stages_str = body
                .get("stages")
                .map(|v| v.to_string())
                .unwrap_or_default();
            tool_ok(format!(
                "已读取最新学习计划《{title}》{img_note}\n- 最新完成进度：{done}/{total}（{pct}%）\n- 专属互动链接：{url}\n- 本机隧道直连备用链接：{local_url}\n- 详细任务状态：{stages_str}"
            ))
        }
        "create_quiz" => {
            let total = body.get("total").and_then(Value::as_i64).unwrap_or(0);
            tool_ok(format!(
                "在线互动测验已生成（共 {total} 题）{img_note}\n- 专属答题链接：{url}\n- 本机隧道直连备用链接：{local_url}\n请在回复中把答题链接发给对方，提醒对方点开即可直接选答案、看提示和实时解析，答完后还能回群让你点评错题。"
            ))
        }
        "get_quiz_result" => {
            if body.get("found").and_then(Value::as_bool) != Some(true) {
                return tool_ok("当前还没有测验记录，可以调用 mode=\"create_quiz\" 出一套测验题。");
            }
            let title = body.get("title").and_then(Value::as_str).unwrap_or("");
            let answered = body
                .get("answered_count")
                .and_then(Value::as_i64)
                .unwrap_or(0);
            let score = body.get("score").and_then(Value::as_i64).unwrap_or(0);
            let total = body.get("total").and_then(Value::as_i64).unwrap_or(0);
            let wrongs = body
                .get("wrong_details")
                .map(|v| v.to_string())
                .unwrap_or_default();
            tool_ok(format!(
                "最近一次测验《{title}》作答结果：\n- 已作答：{answered}/{total} 题 | 答对：{score} 题\n- 错题明细：{wrongs}\n- 原测验链接：{url}"
            ))
        }
        _ => tool_ok("完成"),
    }
}
