#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""
小简学习计划与互动测验中心 (Study Hub Server)
提供两项核心能力：
1. 个性化学习计划互动网页 (/study/plan/<token>) 与高清卡片预览图渲染；
2. 在线互动测验答题网页 (/study/quiz/<token>) 与高清测验卡片预览图渲染；
3. 供机器人后端调用的本地内部接口 (/internal/api/...)，持久化保存在 SQLite。
"""

import base64
import html
import io
import json
import os
import secrets
import sqlite3
import time
from http.server import BaseHTTPRequestHandler, HTTPServer
from socketserver import ThreadingMixIn
from urllib.parse import urlparse


class ThreadingHTTPServer(ThreadingMixIn, HTTPServer):
    daemon_threads = True

from PIL import Image, ImageDraw, ImageFont

LISTEN_HOST = os.environ.get("STUDY_HUB_HOST", "0.0.0.0")
LISTEN_PORT = int(os.environ.get("STUDY_HUB_PORT", "18086"))
PUBLIC_BASE_URL = os.environ.get("STUDY_HUB_PUBLIC_URL", "http://8.218.197.39:18081").rstrip("/")
DB_PATH = os.environ.get("STUDY_HUB_DB", "/opt/gemini-qq-bot/data/study_hub.sqlite3")
INTERNAL_TOKEN = os.environ.get("STUDY_HUB_TOKEN", "xiaojian-study-hub-2026")

FONT_CANDIDATES = [
    "/usr/share/fonts/google-noto-cjk/NotoSansCJK-Regular.ttc",
    "/usr/share/fonts/google-noto-sans-cjk/NotoSansCJK-Regular.ttc",
    "/usr/share/fonts/noto-cjk/NotoSansCJK-Regular.ttc",
    "/usr/share/fonts/wqy-zenhei/wqy-zenhei.ttc",
    "/usr/share/fonts/google-droid/DroidSansFallback.ttf",
    r"C:\Windows\Fonts\msyh.ttc",
    r"C:\Windows\Fonts\simhei.ttf",
]

FONT_BOLD_CANDIDATES = [
    "/usr/share/fonts/google-noto-cjk/NotoSansCJK-Bold.ttc",
    "/usr/share/fonts/google-noto-sans-cjk/NotoSansCJK-Bold.ttc",
    "/usr/share/fonts/noto-cjk/NotoSansCJK-Bold.ttc",
    r"C:\Windows\Fonts\msyhbd.ttc",
] + FONT_CANDIDATES


def _pick_font_path(candidates):
    for p in candidates:
        if os.path.exists(p):
            return p
    return None


REGULAR_FONT_PATH = _pick_font_path(FONT_CANDIDATES)
BOLD_FONT_PATH = _pick_font_path(FONT_BOLD_CANDIDATES)


def load_font(size, bold=False):
    path = BOLD_FONT_PATH if bold else REGULAR_FONT_PATH
    if path:
        try:
            return ImageFont.truetype(path, size)
        except Exception:
            pass
    return ImageFont.load_default()


def init_db():
    os.makedirs(os.path.dirname(os.path.abspath(DB_PATH)), exist_ok=True)
    conn = sqlite3.connect(DB_PATH, timeout=15)
    conn.execute(
        """
        CREATE TABLE IF NOT EXISTS study_plans (
            token TEXT PRIMARY KEY,
            group_id TEXT NOT NULL,
            owner_name TEXT NOT NULL DEFAULT '',
            subject TEXT NOT NULL DEFAULT '',
            title TEXT NOT NULL,
            summary TEXT NOT NULL DEFAULT '',
            stages_json TEXT NOT NULL,
            created_at INTEGER NOT NULL,
            updated_at INTEGER NOT NULL
        )
        """
    )
    conn.execute(
        """
        CREATE TABLE IF NOT EXISTS study_quizzes (
            token TEXT PRIMARY KEY,
            group_id TEXT NOT NULL,
            owner_name TEXT NOT NULL DEFAULT '',
            subject TEXT NOT NULL DEFAULT '',
            title TEXT NOT NULL,
            difficulty TEXT NOT NULL DEFAULT '适中',
            questions_json TEXT NOT NULL,
            answers_json TEXT NOT NULL DEFAULT '{}',
            score INTEGER NOT NULL DEFAULT 0,
            total INTEGER NOT NULL DEFAULT 0,
            submitted_at INTEGER NOT NULL DEFAULT 0,
            created_at INTEGER NOT NULL
        )
        """
    )
    conn.commit()
    conn.close()


def get_conn():
    conn = sqlite3.connect(DB_PATH, timeout=15)
    conn.row_factory = sqlite3.Row
    return conn


# ==================== 高清预览卡片图渲染引擎 ====================

def text_wh(draw, text, font):
    if hasattr(draw, "textbbox"):
        b = draw.textbbox((0, 0), text, font=font)
        return b[2] - b[0], b[3] - b[1]
    return draw.textsize(text, font=font)


def draw_rounded_rect(draw, xy, radius=16, fill=None, outline=None, width=1):
    if hasattr(draw, "rounded_rectangle"):
        draw.rounded_rectangle(xy, radius=radius, fill=fill, outline=outline, width=width)
        return
    x0, y0, x1, y1 = [int(v) for v in xy]
    r = max(0, min(int(radius), (x1 - x0) // 2, (y1 - y0) // 2))
    if fill is not None:
        draw.rectangle([x0 + r, y0, x1 - r, y1], fill=fill)
        draw.rectangle([x0, y0 + r, x1, y1 - r], fill=fill)
        if r > 0:
            draw.pieslice([x0, y0, x0 + 2 * r, y0 + 2 * r], 180, 270, fill=fill)
            draw.pieslice([x1 - 2 * r, y0, x1, y0 + 2 * r], 270, 360, fill=fill)
            draw.pieslice([x0, y1 - 2 * r, x0 + 2 * r, y1], 90, 180, fill=fill)
            draw.pieslice([x1 - 2 * r, y1 - 2 * r, x1, y1], 0, 90, fill=fill)
    if outline is not None:
        draw.line([x0 + r, y0, x1 - r, y0], fill=outline, width=width)
        draw.line([x0 + r, y1, x1 - r, y1], fill=outline, width=width)
        draw.line([x0, y0 + r, x0, y1 - r], fill=outline, width=width)
        draw.line([x1, y0 + r, x1, y1 - r], fill=outline, width=width)
        if r > 0:
            draw.arc([x0, y0, x0 + 2 * r, y0 + 2 * r], 180, 270, fill=outline)
            draw.arc([x1 - 2 * r, y0, x1, y0 + 2 * r], 270, 360, fill=outline)
            draw.arc([x0, y1 - 2 * r, x0 + 2 * r, y1], 90, 180, fill=outline)
            draw.arc([x1 - 2 * r, y1 - 2 * r, x1, y1], 0, 90, fill=outline)


def wrap_text(draw, text, font, max_width):
    lines = []
    for para in str(text or "").split("\n"):
        if not para:
            lines.append("")
            continue
        cur = ""
        for ch in para:
            test = cur + ch
            tw, _ = text_wh(draw, test, font)
            if tw <= max_width or not cur:
                cur = test
            else:
                lines.append(cur)
                cur = ch
        if cur:
            lines.append(cur)
    return lines or [""]


def draw_book_icon(draw, cx, cy, color):
    # 绘制简洁的打开书本图标
    draw_rounded_rect(draw, [cx - 18, cy - 13, cx - 1, cy + 12], radius=3, outline=color, width=3)
    draw_rounded_rect(draw, [cx + 1, cy - 13, cx + 18, cy + 12], radius=3, outline=color, width=3)
    draw.line([cx - 12, cy - 5, cx - 5, cy - 5], fill=color, width=2)
    draw.line([cx - 12, cy + 2, cx - 5, cy + 2], fill=color, width=2)
    draw.line([cx + 5, cy - 5, cx + 12, cy - 5], fill=color, width=2)
    draw.line([cx + 5, cy + 2, cx + 12, cy + 2], fill=color, width=2)


def draw_quiz_icon(draw, cx, cy, color):
    # 绘制简洁的测验举手/勾选图标
    draw.ellipse([cx - 14, cy - 14, cx + 14, cy + 14], outline=color)
    draw.line([cx - 6, cy + 1, cx - 1, cy + 6, cx + 8, cy - 5], fill=color, width=3)


def compute_plan_stats(stages):
    total = 0
    done = 0
    for st in stages:
        for tk in st.get("tasks", []):
            total += 1
            if tk.get("done"):
                done += 1
    pct = int(round((done / total) * 100)) if total > 0 else 0
    return done, total, pct


def render_plan_card_b64(title, subject, summary, stages):
    W = 920
    pad = 44
    card_x0, card_x1 = 36, W - 36
    inner_w = (card_x1 - card_x0) - pad * 2

    f_badge = load_font(24, bold=True)
    f_title = load_font(38, bold=True)
    f_sub = load_font(23, bold=False)
    f_stage = load_font(26, bold=True)
    f_task = load_font(23, bold=False)
    f_cta = load_font(24, bold=True)

    # 先在临时画布上计算实际内容高度
    tmp_img = Image.new("RGB", (W, 100), "#F2F4F8")
    tmp_draw = ImageDraw.Draw(tmp_img)

    title_lines = wrap_text(tmp_draw, title, f_title, inner_w)[:2]
    summary_lines = wrap_text(tmp_draw, summary, f_sub, inner_w)[:2] if summary else []

    preview_stages = []
    for st in stages[:3]:
        st_title = str(st.get("stage_title") or "学习阶段")
        tasks = st.get("tasks", [])[:3]
        t_items = []
        for tk in tasks:
            t_lines = wrap_text(tmp_draw, str(tk.get("text") or ""), f_task, inner_w - 56)[:2]
            t_items.append((bool(tk.get("done")), t_lines))
        preview_stages.append((st_title, t_items, len(st.get("tasks", []))))

    # 动态计算总高度
    y = 36 + 40
    y += 58
    y += len(title_lines) * 50 + 12
    if summary_lines:
        y += len(summary_lines) * 34 + 14
    y += 96
    for _, t_items, _ in preview_stages:
        y += 46
        for _, t_lines in t_items:
            y += len(t_lines) * 33 + 12
        y += 18
    if len(stages) > 3:
        y += 38
    y += 88
    H = max(560, y + 52)

    img = Image.new("RGB", (W, H), "#EEF2F7")
    draw = ImageDraw.Draw(img)

    # 卡片主体与轻微阴影
    draw_rounded_rect(draw, [card_x0 + 3, 40, card_x1 + 3, H - 32], radius=28, fill="#DCE3EE")
    draw_rounded_rect(draw, [card_x0, 36, card_x1, H - 36], radius=28, fill="#FFFFFF", outline="#E2E8F0", width=2)

    cy = 36 + 38
    cx = card_x0 + pad

    # 顶部胶囊徽章
    draw_rounded_rect(draw, [cx, cy, cx + 340, cy + 48], radius=24, fill="#EEF4FF")
    draw_book_icon(draw, cx + 32, cy + 24, "#2563EB")
    draw.text((cx + 62, cy + 10), "查看你的个性化学习计划", font=f_badge, fill="#1D4ED8")

    if subject:
        sb_w0, _ = text_wh(draw, subject, f_sub)
        sb_w = sb_w0 + 34
        draw_rounded_rect(draw, [card_x1 - pad - sb_w, cy + 4, card_x1 - pad, cy + 44], radius=20, fill="#F1F5F9")
        draw.text((card_x1 - pad - sb_w + 17, cy + 10), subject, font=f_sub, fill="#475569")

    cy += 68

    # 主标题
    for ln in title_lines:
        draw.text((cx, cy), ln, font=f_title, fill="#0F172A")
        cy += 50
    cy += 6

    if summary_lines:
        for ln in summary_lines:
            draw.text((cx, cy), ln, font=f_sub, fill="#64748B")
            cy += 34
        cy += 12

    # 进度条区块
    done_cnt, total_cnt, pct = compute_plan_stats(stages)
    draw_rounded_rect(draw, [cx, cy, card_x1 - pad, cy + 78], radius=18, fill="#F8FAFC", outline="#E2E8F0", width=1)
    draw.text((cx + 22, cy + 14), f"当前学习进度：{done_cnt} / {total_cnt} 项已完成", font=f_sub, fill="#334155")
    pct_str = f"{pct}%"
    pw, _ = text_wh(draw, pct_str, f_badge)
    draw.text((card_x1 - pad - 22 - pw, cy + 12), pct_str, font=f_badge, fill="#2563EB")

    bar_x0, bar_x1 = cx + 22, card_x1 - pad - 22
    bar_y0, bar_y1 = cy + 50, cy + 64
    draw_rounded_rect(draw, [bar_x0, bar_y0, bar_x1, bar_y1], radius=7, fill="#E2E8F0")
    fill_w = max(14, int((bar_x1 - bar_x0) * pct / 100)) if pct > 0 else 0
    if fill_w > 0:
        draw_rounded_rect(draw, [bar_x0, bar_y0, bar_x0 + fill_w, bar_y1], radius=7, fill="#3B82F6")
    cy += 98

    # 阶段与任务清单预览
    for idx, (st_title, t_items, total_in_stage) in enumerate(preview_stages, 1):
        draw_rounded_rect(draw, [cx, cy, cx + 6, cy + 28], radius=3, fill="#3B82F6")
        draw.text((cx + 18, cy), f"{st_title}", font=f_stage, fill="#1E293B")
        cy += 42
        for is_done, t_lines in t_items:
            if is_done:
                draw.ellipse([cx + 14, cy + 4, cx + 38, cy + 28], fill="#10B981")
                draw.line([cx + 20, cy + 16, cx + 24, cy + 21, cx + 32, cy + 11], fill="#FFFFFF", width=3)
            else:
                draw.ellipse([cx + 14, cy + 4, cx + 38, cy + 28], outline="#94A3B8")
            text_color = "#94A3B8" if is_done else "#334155"
            for ln in t_lines:
                draw.text((cx + 52, cy), ln, font=f_task, fill=text_color)
                cy += 33
            cy += 10
        if total_in_stage > len(t_items):
            draw.text((cx + 52, cy - 4), f"... 本阶段共 {total_in_stage} 项任务，点链接展开全部", font=load_font(20), fill="#94A3B8")
            cy += 24
        cy += 12

    if len(stages) > 3:
        draw.text((cx, cy), f"还有 {len(stages) - 3} 个后续阶段未展示，点击链接查看完整路线图", font=f_sub, fill="#64748B")
        cy += 36

    # 底部引导栏
    cta_y0 = H - 36 - 88
    draw_rounded_rect(draw, [cx, cta_y0, card_x1 - pad, cta_y0 + 58], radius=16, fill="#2563EB")
    cta_text = "点击下方链接，直接在线勾选完成进度与查看学习要点  >"
    cw, _ = text_wh(draw, cta_text, f_cta)
    draw.text((cx + (inner_w - cw) // 2, cta_y0 + 15), cta_text, font=f_cta, fill="#FFFFFF")

    buf = io.BytesIO()
    img.save(buf, format="PNG", optimize=True)
    return base64.b64encode(buf.getvalue()).decode("ascii")


def render_quiz_card_b64(title, subject, difficulty, questions):
    W = 920
    pad = 44
    card_x0, card_x1 = 36, W - 36
    inner_w = (card_x1 - card_x0) - pad * 2

    f_badge = load_font(24, bold=True)
    f_title = load_font(38, bold=True)
    f_sub = load_font(23, bold=False)
    f_q = load_font(26, bold=True)
    f_opt = load_font(23, bold=False)
    f_cta = load_font(24, bold=True)

    tmp_img = Image.new("RGB", (W, 100), "#F2F4F8")
    tmp_draw = ImageDraw.Draw(tmp_img)

    title_lines = wrap_text(tmp_draw, title, f_title, inner_w)[:2]
    q0 = questions[0] if questions else {}
    q0_lines = wrap_text(tmp_draw, f"Q1. {q0.get('question', '')}", f_q, inner_w - 48)[:3]
    opts_wrapped = []
    letters = ["A", "B", "C", "D"]
    for idx, opt in enumerate((q0.get("options") or [])[:4]):
        ol = wrap_text(tmp_draw, f"{letters[idx]}. {opt}", f_opt, inner_w - 86)[:2]
        opts_wrapped.append(ol)

    # 动态高度
    y = 36 + 38 + 68 + len(title_lines) * 50 + 54
    q_box_h = 28 + len(q0_lines) * 38 + 18
    for ol in opts_wrapped:
        q_box_h += max(50, len(ol) * 32 + 20) + 12
    q_box_h += 18
    H = max(620, y + q_box_h + 120)

    img = Image.new("RGB", (W, H), "#EEF3F9")
    draw = ImageDraw.Draw(img)

    draw_rounded_rect(draw, [card_x0 + 3, 40, card_x1 + 3, H - 32], radius=28, fill="#D8E1EE")
    draw_rounded_rect(draw, [card_x0, 36, card_x1, H - 36], radius=28, fill="#FFFFFF", outline="#E2E8F0", width=2)

    cy = 36 + 38
    cx = card_x0 + pad

    # 顶部胶囊徽章
    draw_rounded_rect(draw, [cx, cy, cx + 340, cy + 48], radius=24, fill="#F3E8FF")
    draw_quiz_icon(draw, cx + 32, cy + 24, "#7C3AED")
    draw.text((cx + 62, cy + 10), "通过测验检验学习成果", font=f_badge, fill="#6D28D9")

    meta_text = f"共 {len(questions)} 题 · {difficulty or '适中'}"
    mw0, _ = text_wh(draw, meta_text, f_sub)
    mw = mw0 + 34
    draw_rounded_rect(draw, [card_x1 - pad - mw, cy + 4, card_x1 - pad, cy + 44], radius=20, fill="#F1F5F9")
    draw.text((card_x1 - pad - mw + 17, cy + 10), meta_text, font=f_sub, fill="#475569")

    cy += 68
    for ln in title_lines:
        draw.text((cx, cy), ln, font=f_title, fill="#0F172A")
        cy += 50

    if subject:
        draw.text((cx, cy + 2), f"测验主题：{subject}   |   支持实时判分、提示与逐题解析", font=f_sub, fill="#64748B")
    cy += 46

    # 第1题预览卡片框
    draw_rounded_rect(draw, [cx, cy, card_x1 - pad, cy + q_box_h], radius=20, fill="#F8FAFC", outline="#E2E8F0", width=2)
    qy = cy + 24
    qx = cx + 24
    for ln in q0_lines:
        draw.text((qx, qy), ln, font=f_q, fill="#1E293B")
        qy += 38
    qy += 14

    for ol in opts_wrapped:
        oh = max(50, len(ol) * 32 + 20)
        draw_rounded_rect(draw, [qx, qy, card_x1 - pad - 24, qy + oh], radius=14, fill="#FFFFFF", outline="#CBD5E1", width=2)
        oy = qy + 11
        for ln in ol:
            draw.text((qx + 20, oy), ln, font=f_opt, fill="#334155")
            oy += 32
        qy += oh + 12

    # 底部按钮条
    cta_y0 = H - 36 - 86
    draw_rounded_rect(draw, [cx, cta_y0, card_x1 - pad, cta_y0 + 58], radius=16, fill="#7C3AED")
    cta_text = f"点击下方链接开始在线答题（共 {len(questions)} 题，点选即出解析）  >"
    cw, _ = text_wh(draw, cta_text, f_cta)
    draw.text((cx + (inner_w - cw) // 2, cta_y0 + 15), cta_text, font=f_cta, fill="#FFFFFF")

    buf = io.BytesIO()
    img.save(buf, format="PNG", optimize=True)
    return base64.b64encode(buf.getvalue()).decode("ascii")


# ==================== 互动 HTML 页面模板 ====================

def render_plan_html(row):
    token = row["token"]
    title = html.escape(row["title"])
    subject = html.escape(row["subject"] or "个性化学习")
    summary = html.escape(row["summary"] or "")
    stages_json = row["stages_json"]
    return f"""<!DOCTYPE html>
<html lang="zh-CN">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width,initial-scale=1,maximum-scale=1">
<title>{title} - 个性化学习计划</title>
<style>
  :root {{ --bg:#f4f6fb; --card:#ffffff; --primary:#2563eb; --primary-soft:#eff6ff; --green:#10b981; --text:#0f172a; --muted:#64748b; --border:#e2e8f0; }}
  * {{ box-sizing:border-box; }}
  body {{ margin:0; font-family:-apple-system,BlinkMacSystemFont,"PingFang SC","Microsoft YaHei",sans-serif; background:var(--bg); color:var(--text); line-height:1.6; }}
  .wrap {{ max-width:760px; margin:0 auto; padding:20px 16px 60px; }}
  .header-card {{ background:var(--card); border-radius:22px; padding:24px; box-shadow:0 6px 24px rgba(15,23,42,.06); border:1px solid var(--border); margin-bottom:18px; }}
  .badge {{ display:inline-flex; align-items:center; gap:8px; background:var(--primary-soft); color:var(--primary); padding:6px 14px; border-radius:999px; font-size:13px; font-weight:600; }}
  h1 {{ margin:14px 0 8px; font-size:24px; line-height:1.35; }}
  .summary {{ margin:0 0 18px; color:var(--muted); font-size:14px; }}
  .prog-row {{ display:flex; justify-content:space-between; align-items:center; font-size:14px; margin-bottom:8px; font-weight:600; }}
  .prog-bar {{ height:10px; background:#e2e8f0; border-radius:999px; overflow:hidden; }}
  .prog-fill {{ height:100%; background:linear-gradient(90deg,#3b82f6,#2563eb); border-radius:999px; transition:width .35s ease; }}
  .stage-card {{ background:var(--card); border-radius:20px; padding:20px; margin-bottom:16px; border:1px solid var(--border); box-shadow:0 4px 16px rgba(15,23,42,.04); }}
  .stage-head {{ display:flex; justify-content:space-between; align-items:center; margin-bottom:12px; }}
  .stage-title {{ font-size:17px; font-weight:700; display:flex; align-items:center; gap:10px; }}
  .stage-title::before {{ content:""; width:4px; height:18px; background:var(--primary); border-radius:4px; }}
  .stage-count {{ font-size:12px; color:var(--muted); background:#f1f5f9; padding:3px 10px; border-radius:999px; }}
  .task-item {{ display:flex; align-items:flex-start; gap:12px; padding:12px; border-radius:14px; border:1px solid #f1f5f9; margin-bottom:10px; transition:all .2s; cursor:pointer; background:#fafcff; }}
  .task-item:hover {{ border-color:#bfdbfe; background:#f0f7ff; }}
  .task-item.done {{ background:#f8fafc; opacity:.78; }}
  .check {{ width:22px; height:22px; border-radius:50%; border:2px solid #94a3b8; flex-shrink:0; margin-top:2px; display:flex; align-items:center; justify-content:center; transition:all .2s; }}
  .task-item.done .check {{ background:var(--green); border-color:var(--green); color:#fff; }}
  .task-body {{ flex:1; }}
  .task-text {{ font-size:15px; font-weight:500; }}
  .task-item.done .task-text {{ text-decoration:line-through; color:var(--muted); }}
  .task-tip {{ font-size:13px; color:var(--muted); margin-top:6px; padding:8px 12px; background:#ffffff; border-radius:10px; border:1px dashed #cbd5e1; }}
  .toast {{ position:fixed; bottom:24px; left:50%; transform:translateX(-50%); background:#0f172a; color:#fff; padding:10px 20px; border-radius:999px; font-size:13px; opacity:0; pointer-events:none; transition:opacity .25s; }}
  .toast.show {{ opacity:1; }}
  .actions {{ display:flex; gap:10px; margin-top:14px; flex-wrap:wrap; }}
  .btn {{ border:none; border-radius:12px; padding:10px 16px; font-size:14px; font-weight:600; cursor:pointer; }}
  .btn-primary {{ background:var(--primary); color:#fff; }}
  .btn-ghost {{ background:#f1f5f9; color:#334155; }}
</style>
</head>
<body>
<div class="wrap">
  <div class="header-card">
    <div class="badge">个性化学习计划 · {subject}</div>
    <h1>{title}</h1>
    <p class="summary">{summary}</p>
    <div class="prog-row"><span id="progLabel">加载进度中...</span><span id="progPct" style="color:var(--primary)">0%</span></div>
    <div class="prog-bar"><div class="prog-fill" id="progFill" style="width:0%"></div></div>
    <div class="actions">
      <button class="btn btn-primary" onclick="copySummary()">复制当前进度回群汇报</button>
    </div>
  </div>
  <div id="stagesRoot"></div>
</div>
<div class="toast" id="toast">已保存进度</div>
<script>
const TOKEN = {json.dumps(token)};
let stages = {stages_json};

function showToast(msg) {{
  const t = document.getElementById('toast');
  t.textContent = msg;
  t.classList.add('show');
  setTimeout(() => t.classList.remove('show'), 1600);
}}

function render() {{
  let total = 0, done = 0;
  const root = document.getElementById('stagesRoot');
  root.innerHTML = '';
  stages.forEach((st, sIdx) => {{
    const tasks = st.tasks || [];
    let sDone = 0;
    tasks.forEach(t => {{ total++; if (t.done) {{ done++; sDone++; }} }});
    const card = document.createElement('div');
    card.className = 'stage-card';
    card.innerHTML = `<div class="stage-head"><div class="stage-title">${{escapeHtml(st.stage_title || '阶段 ' + (sIdx+1))}}</div><div class="stage-count">${{sDone}} / ${{tasks.length}}</div></div>`;
    tasks.forEach((tk, tIdx) => {{
      const item = document.createElement('div');
      item.className = 'task-item' + (tk.done ? ' done' : '');
      item.onclick = () => toggleTask(sIdx, tIdx);
      const tipHtml = tk.tip ? `<div class="task-tip">学习要点：${{escapeHtml(tk.tip)}}</div>` : '';
      item.innerHTML = `<div class="check">${{tk.done ? '✓' : ''}}</div><div class="task-body"><div class="task-text">${{escapeHtml(tk.text || '')}}</div>${{tipHtml}}</div>`;
      card.appendChild(item);
    }});
    root.appendChild(card);
  }});
  const pct = total ? Math.round((done / total) * 100) : 0;
  document.getElementById('progLabel').textContent = `已完成 ${{done}} / ${{total}} 项任务`;
  document.getElementById('progPct').textContent = pct + '%';
  document.getElementById('progFill').style.width = pct + '%';
}}

function escapeHtml(s) {{
  return String(s).replace(/&/g,'&amp;').replace(/</g,'&lt;').replace(/>/g,'&gt;');
}}

async function toggleTask(sIdx, tIdx) {{
  stages[sIdx].tasks[tIdx].done = !stages[sIdx].tasks[tIdx].done;
  render();
  try {{
    await fetch(`/study/api/plan/${{TOKEN}}/toggle`, {{
      method: 'POST',
      headers: {{'Content-Type': 'application/json'}},
      body: JSON.stringify({{stages}})
    }});
    showToast('进度已实时同步给机器人');
  }} catch (e) {{
    showToast('保存失败，请检查网络');
  }}
}}

function copySummary() {{
  let total = 0, done = 0;
  stages.forEach(s => (s.tasks||[]).forEach(t => {{ total++; if(t.done) done++; }}));
  const txt = `我刚更新了学习计划《{title}》，目前已完成 ${{done}}/${{total}} 项，帮我看看下一步学什么或出套测验检验一下吧！`;
  if (navigator.clipboard) {{
    navigator.clipboard.writeText(txt).then(() => showToast('已复制进度，粘贴回QQ群即可'));
  }} else {{
    prompt('复制下方文字发到群里：', txt);
  }}
}}
render();
</script>
</body>
</html>"""


def render_quiz_html(row):
    token = row["token"]
    title = html.escape(row["title"])
    subject = html.escape(row["subject"] or "知识测验")
    difficulty = html.escape(row["difficulty"] or "适中")
    questions_json = row["questions_json"]
    answers_json = row["answers_json"] or "{}"
    return f"""<!DOCTYPE html>
<html lang="zh-CN">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width,initial-scale=1,maximum-scale=1">
<title>{title} - 在线互动测验</title>
<style>
  :root {{ --bg:#f3f4fa; --card:#ffffff; --primary:#7c3aed; --primary-soft:#f3e8ff; --green:#10b981; --green-soft:#ecfdf5; --red:#ef4444; --red-soft:#fef2f2; --text:#0f172a; --muted:#64748b; --border:#e2e8f0; }}
  * {{ box-sizing:border-box; }}
  body {{ margin:0; font-family:-apple-system,BlinkMacSystemFont,"PingFang SC","Microsoft YaHei",sans-serif; background:var(--bg); color:var(--text); line-height:1.65; }}
  .wrap {{ max-width:740px; margin:0 auto; padding:20px 16px 64px; }}
  .top-bar {{ display:flex; justify-content:space-between; align-items:center; margin-bottom:14px; gap:10px; flex-wrap:wrap; }}
  .badge {{ background:var(--primary-soft); color:var(--primary); padding:5px 14px; border-radius:999px; font-size:13px; font-weight:600; }}
  .dots {{ display:flex; gap:6px; flex-wrap:wrap; }}
  .dot {{ width:28px; height:28px; border-radius:50%; background:#e2e8f0; color:#475569; font-size:13px; font-weight:700; display:flex; align-items:center; justify-content:center; cursor:pointer; border:2px solid transparent; }}
  .dot.active {{ border-color:var(--primary); background:#fff; color:var(--primary); }}
  .dot.right {{ background:var(--green); color:#fff; }}
  .dot.wrong {{ background:var(--red); color:#fff; }}
  .card {{ background:var(--card); border-radius:24px; padding:24px; box-shadow:0 8px 28px rgba(15,23,42,.06); border:1px solid var(--border); }}
  .q-meta {{ font-size:13px; color:var(--muted); margin-bottom:10px; display:flex; justify-content:space-between; }}
  .q-text {{ font-size:19px; font-weight:700; margin:0 0 20px; white-space:pre-wrap; }}
  .opt-btn {{ width:100%; text-align:left; background:#fff; border:2px solid #e2e8f0; border-radius:16px; padding:14px 16px; margin-bottom:12px; font-size:15px; color:var(--text); cursor:pointer; display:flex; align-items:flex-start; gap:12px; transition:all .18s; }}
  .opt-btn:hover:not(:disabled) {{ border-color:#c4b5fd; background:#faf5ff; }}
  .opt-btn:disabled {{ cursor:default; }}
  .opt-letter {{ width:28px; height:28px; border-radius:50%; background:#f1f5f9; color:#334155; font-weight:700; display:flex; align-items:center; justify-content:center; flex-shrink:0; font-size:14px; }}
  .opt-btn.correct {{ border-color:var(--green); background:var(--green-soft); }}
  .opt-btn.correct .opt-letter {{ background:var(--green); color:#fff; }}
  .opt-btn.wrong {{ border-color:var(--red); background:var(--red-soft); }}
  .opt-btn.wrong .opt-letter {{ background:var(--red); color:#fff; }}
  .hint-box {{ margin-top:12px; padding:12px 16px; background:#fffbeb; border:1px solid #fde68a; border-radius:14px; font-size:14px; color:#92400e; display:none; }}
  .exp-box {{ margin-top:16px; padding:16px; background:#f8fafc; border-radius:16px; border-left:4px solid var(--primary); font-size:14px; display:none; }}
  .exp-box.show {{ display:block; }}
  .nav-row {{ display:flex; justify-content:space-between; align-items:center; margin-top:22px; gap:10px; }}
  .btn {{ border:none; border-radius:14px; padding:11px 18px; font-size:14px; font-weight:600; cursor:pointer; }}
  .btn-primary {{ background:var(--primary); color:#fff; }}
  .btn-ghost {{ background:#f1f5f9; color:#334155; }}
  .btn-outline {{ background:#fff; border:1px solid #cbd5e1; color:#475569; }}
  .result-card {{ text-align:center; padding:12px 4px; }}
  .score-big {{ font-size:52px; font-weight:800; color:var(--primary); line-height:1.1; margin:12px 0; }}
  .stat-grid {{ display:grid; grid-template-columns:repeat(3,1fr); gap:10px; margin:20px 0; }}
  .stat-item {{ background:#f8fafc; border-radius:16px; padding:14px 8px; border:1px solid var(--border); }}
  .stat-num {{ font-size:22px; font-weight:700; }}
  .stat-lbl {{ font-size:12px; color:var(--muted); }}
  .wrong-list {{ text-align:left; margin-top:18px; }}
  .wrong-item {{ background:#fef2f2; border:1px solid #fecaca; border-radius:14px; padding:14px; margin-bottom:10px; font-size:14px; }}
</style>
</head>
<body>
<div class="wrap">
  <div class="top-bar">
    <div class="badge">{subject} · {difficulty}</div>
    <div class="dots" id="dots"></div>
  </div>
  <div class="card" id="mainCard"></div>
</div>
<script>
const TOKEN = {json.dumps(token)};
const TITLE = {json.dumps(row["title"])};
const QUESTIONS = {questions_json};
let answers = {answers_json};
let curIdx = 0;
let showSummary = false;
const LETTERS = ['A','B','C','D'];

function esc(s) {{
  return String(s ?? '').replace(/&/g,'&amp;').replace(/</g,'&lt;').replace(/>/g,'&gt;');
}}

function renderDots() {{
  const box = document.getElementById('dots');
  box.innerHTML = '';
  QUESTIONS.forEach((q, i) => {{
    const d = document.createElement('div');
    d.className = 'dot';
    if (!showSummary && i === curIdx) d.classList.add('active');
    if (answers[String(i)] !== undefined) {{
      d.classList.add(answers[String(i)] === q.answer ? 'right' : 'wrong');
    }}
    d.textContent = i + 1;
    d.onclick = () => {{ showSummary = false; curIdx = i; render(); }};
    box.appendChild(d);
  }});
}}

function render() {{
  renderDots();
  const card = document.getElementById('mainCard');
  if (showSummary) {{
    let right = 0, wrongItems = [];
    QUESTIONS.forEach((q, i) => {{
      const chosen = answers[String(i)];
      if (chosen === q.answer) right++;
      else wrongItems.push({{i, q, chosen}});
    }});
    const total = QUESTIONS.length;
    const pct = total ? Math.round((right / total) * 100) : 0;
    let wrongHtml = '';
    if (wrongItems.length) {{
      wrongHtml = '<div class="wrong-list"><h3>错题复盘</h3>' + wrongItems.map(w => `
        <div class="wrong-item">
          <div><strong>第${{w.i+1}}题：</strong>${{esc(w.q.question)}}</div>
          <div style="margin-top:6px;color:#b91c1c;">你的选择：${{w.chosen !== undefined ? LETTERS[w.chosen] + '. ' + esc(w.q.options[w.chosen]) : '未作答'}}</div>
          <div style="color:#047857;">正确答案：${{LETTERS[w.q.answer]}}. ${{esc(w.q.options[w.q.answer])}}</div>
          <div style="margin-top:6px;color:#334155;">解析：${{esc(w.q.explanation)}}</div>
        </div>
      `).join('') + '</div>';
    }} else {{
      wrongHtml = '<p style="color:#059669;font-weight:600;margin-top:16px;">全部答对！这项知识点已经掌握得很扎实了！</p>';
    }}
    card.innerHTML = `
      <div class="result-card">
        <div class="badge">测验成绩单</div>
        <div class="score-big">${{pct}}%</div>
        <div style="color:var(--muted);font-size:14px;">《${{esc(TITLE)}}》成绩已自动同步给机器人</div>
        <div class="stat-grid">
          <div class="stat-item"><div class="stat-num">${{total}}</div><div class="stat-lbl">总题数</div></div>
          <div class="stat-item"><div class="stat-num" style="color:var(--green)">${{right}}</div><div class="stat-lbl">答对</div></div>
          <div class="stat-item"><div class="stat-num" style="color:var(--red)">${{total - right}}</div><div class="stat-lbl">错题</div></div>
        </div>
        <div style="display:flex;gap:10px;justify-content:center;flex-wrap:wrap;">
          <button class="btn btn-primary" onclick="copyReport(${{right}}, ${{total}}, ${{pct}})">复制成绩单发回QQ群</button>
          <button class="btn btn-ghost" onclick="showSummary=false;curIdx=0;render();">返回逐题查看</button>
        </div>
        ${{wrongHtml}}
      </div>
    `;
    return;
  }}

  const q = QUESTIONS[curIdx];
  const chosen = answers[String(curIdx)];
  const answered = chosen !== undefined;
  const optsHtml = (q.options || []).map((opt, idx) => {{
    let cls = 'opt-btn';
    if (answered) {{
      if (idx === q.answer) cls += ' correct';
      else if (idx === chosen) cls += ' wrong';
    }}
    return `<button class="${{cls}}" ${{answered ? 'disabled' : ''}} onclick="choose(${{idx}})"><span class="opt-letter">${{LETTERS[idx]}}</span><span>${{esc(opt)}}</span></button>`;
  }}).join('');

  const allDone = Object.keys(answers).length >= QUESTIONS.length;
  const hintBtn = (!answered && q.hint) ? `<button class="btn btn-outline" onclick="document.getElementById('hintBox').style.display='block'">查看提示</button>` : '<span></span>';
  const nextBtn = curIdx < QUESTIONS.length - 1
    ? `<button class="btn btn-primary" onclick="curIdx++;render();">下一题 →</button>`
    : `<button class="btn btn-primary" onclick="showSummary=true;render();">查看成绩单</button>`;

  card.innerHTML = `
    <div class="q-meta"><span>第 ${{curIdx + 1}} / ${{QUESTIONS.length}} 题</span><span>${{esc(q.tag || '')}}</span></div>
    <div class="q-text">${{esc(q.question)}}</div>
    <div>${{optsHtml}}</div>
    <div class="hint-box" id="hintBox">提示：${{esc(q.hint || '')}}</div>
    <div class="exp-box ${{answered ? 'show' : ''}}">
      <div style="font-weight:700;margin-bottom:4px;color:${{chosen === q.answer ? '#059669' : '#dc2626'}}">${{chosen === q.answer ? '回答正确！' : '回答有误，正确答案是 ' + LETTERS[q.answer]}}</div>
      <div>${{esc(q.explanation || '暂无解析')}}</div>
    </div>
    <div class="nav-row">
      <div>${{curIdx > 0 ? '<button class="btn btn-ghost" onclick="curIdx--;render();">← 上一题</button>' : ''}} ${{hintBtn}}</div>
      <div style="display:flex;gap:8px;">
        ${{allDone && curIdx < QUESTIONS.length - 1 ? '<button class="btn btn-outline" onclick="showSummary=true;render();">成绩单</button>' : ''}}
        ${{nextBtn}}
      </div>
    </div>
  `;
}}

async function choose(idx) {{
  answers[String(curIdx)] = idx;
  render();
  let score = 0;
  QUESTIONS.forEach((q, i) => {{ if (answers[String(i)] === q.answer) score++; }});
  try {{
    await fetch(`/study/api/quiz/${{TOKEN}}/answer`, {{
      method: 'POST',
      headers: {{'Content-Type': 'application/json'}},
      body: JSON.stringify({{answers, score, total: QUESTIONS.length}})
    }});
  }} catch (e) {{}}
}}

function copyReport(right, total, pct) {{
  const txt = `我刚做完测验《${{TITLE}}》，答对 ${{right}}/${{total}} 题（正确率 ${{pct}}%），帮我点评一下错题并安排下一步吧！`;
  if (navigator.clipboard) {{
    navigator.clipboard.writeText(txt).then(() => alert('已复制成绩单，粘贴回QQ群即可'));
  }} else {{
    prompt('复制下方成绩单发到群里：', txt);
  }}
}}
render();
</script>
</body>
</html>"""


# ==================== HTTP 服务路由 ====================

class StudyHubHandler(BaseHTTPRequestHandler):
    def log_message(self, fmt, *args):
        pass

    def _send_json(self, code, obj):
        data = json.dumps(obj, ensure_ascii=False).encode("utf-8")
        self.send_response(code)
        self.send_header("Content-Type", "application/json; charset=utf-8")
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)

    def _send_html(self, code, content):
        data = content.encode("utf-8")
        self.send_response(code)
        self.send_header("Content-Type", "text/html; charset=utf-8")
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)

    def _read_json(self):
        length = int(self.headers.get("Content-Length", "0"))
        raw = self.rfile.read(length) if length > 0 else b"{}"
        try:
            return json.loads(raw.decode("utf-8"))
        except Exception:
            return {}

    def do_GET(self):
        path = urlparse(self.path).path.rstrip("/")
        if path in ("", "/health", "/study/health"):
            return self._send_json(200, {"ok": True, "service": "study_hub"})

        if path.startswith("/study/plan/"):
            token = path.split("/study/plan/", 1)[1].strip("/")
            conn = get_conn()
            row = conn.execute("SELECT * FROM study_plans WHERE token = ?", (token,)).fetchone()
            conn.close()
            if not row:
                return self._send_html(404, "<h2>未找到对应的学习计划，请让机器人重新生成。</h2>")
            return self._send_html(200, render_plan_html(row))

        if path.startswith("/study/quiz/"):
            token = path.split("/study/quiz/", 1)[1].strip("/")
            conn = get_conn()
            row = conn.execute("SELECT * FROM study_quizzes WHERE token = ?", (token,)).fetchone()
            conn.close()
            if not row:
                return self._send_html(404, "<h2>未找到对应的测验卷，请让机器人重新出题。</h2>")
            return self._send_html(200, render_quiz_html(row))

        return self._send_json(404, {"ok": False, "error": "not_found"})

    def do_POST(self):
        path = urlparse(self.path).path.rstrip("/")
        body = self._read_json()

        # 公开网页端交互保存接口（靠 URL 中的随机 token 鉴权）
        if path.startswith("/study/api/plan/") and path.endswith("/toggle"):
            token = path[len("/study/api/plan/"):-len("/toggle")].strip("/")
            stages = body.get("stages")
            if not isinstance(stages, list):
                return self._send_json(400, {"ok": False, "error": "invalid_stages"})
            conn = get_conn()
            cur = conn.execute(
                "UPDATE study_plans SET stages_json = ?, updated_at = ? WHERE token = ?",
                (json.dumps(stages, ensure_ascii=False), int(time.time()), token),
            )
            conn.commit()
            conn.close()
            return self._send_json(200, {"ok": cur.rowcount > 0})

        if path.startswith("/study/api/quiz/") and path.endswith("/answer"):
            token = path[len("/study/api/quiz/"):-len("/answer")].strip("/")
            answers = body.get("answers")
            if not isinstance(answers, dict):
                return self._send_json(400, {"ok": False, "error": "invalid_answers"})
            score = int(body.get("score", 0))
            total = int(body.get("total", 0))
            conn = get_conn()
            cur = conn.execute(
                "UPDATE study_quizzes SET answers_json = ?, score = ?, total = ?, submitted_at = ? WHERE token = ?",
                (json.dumps(answers, ensure_ascii=False), score, total, int(time.time()), token),
            )
            conn.commit()
            conn.close()
            return self._send_json(200, {"ok": cur.rowcount > 0})

        # 内部接口（仅限本机机器人调用）
        if path.startswith("/internal/api/"):
            if self.headers.get("X-Study-Token", "") != INTERNAL_TOKEN:
                return self._send_json(403, {"ok": False, "error": "forbidden"})

            if path == "/internal/api/plan/create":
                group_id = str(body.get("group_id") or "default")
                owner_name = str(body.get("owner_name") or "")
                subject = str(body.get("subject") or "个性化学习")
                title = str(body.get("title") or f"{subject}学习计划")
                summary = str(body.get("summary") or "")
                raw_stages = body.get("stages") or []
                norm_stages = []
                for s_idx, st in enumerate(raw_stages, 1):
                    st_title = str(st.get("stage_title") or st.get("title") or f"阶段 {s_idx}")
                    tasks = []
                    for t_idx, tk in enumerate(st.get("tasks") or [], 1):
                        if isinstance(tk, str):
                            tasks.append({"id": f"s{s_idx}t{t_idx}", "text": tk, "tip": "", "done": False})
                        elif isinstance(tk, dict):
                            tasks.append({
                                "id": str(tk.get("id") or f"s{s_idx}t{t_idx}"),
                                "text": str(tk.get("text") or tk.get("title") or ""),
                                "tip": str(tk.get("tip") or ""),
                                "done": bool(tk.get("done", False)),
                            })
                    norm_stages.append({"stage_title": st_title, "tasks": tasks})

                token = secrets.token_urlsafe(9)
                now = int(time.time())
                conn = get_conn()
                conn.execute(
                    "INSERT INTO study_plans (token, group_id, owner_name, subject, title, summary, stages_json, created_at, updated_at) VALUES (?,?,?,?,?,?,?,?,?)",
                    (token, group_id, owner_name, subject, title, summary, json.dumps(norm_stages, ensure_ascii=False), now, now),
                )
                conn.commit()
                conn.close()
                img_b64 = render_plan_card_b64(title, subject, summary, norm_stages)
                done_cnt, total_cnt, pct = compute_plan_stats(norm_stages)
                return self._send_json(200, {
                    "ok": True,
                    "token": token,
                    "url": f"{PUBLIC_BASE_URL}/study/plan/{token}",
                    "local_url": f"http://127.0.0.1:18081/study/plan/{token}",
                    "done": done_cnt,
                    "total": total_cnt,
                    "pct": pct,
                    "image_base64": img_b64,
                })

            if path == "/internal/api/plan/latest":
                group_id = str(body.get("group_id") or "default")
                conn = get_conn()
                row = conn.execute(
                    "SELECT * FROM study_plans WHERE group_id = ? ORDER BY updated_at DESC LIMIT 1",
                    (group_id,),
                ).fetchone()
                conn.close()
                if not row:
                    return self._send_json(200, {"ok": True, "found": False})
                stages = json.loads(row["stages_json"])
                done_cnt, total_cnt, pct = compute_plan_stats(stages)
                img_b64 = render_plan_card_b64(row["title"], row["subject"], row["summary"], stages)
                return self._send_json(200, {
                    "ok": True,
                    "found": True,
                    "token": row["token"],
                    "title": row["title"],
                    "subject": row["subject"],
                    "summary": row["summary"],
                    "stages": stages,
                    "done": done_cnt,
                    "total": total_cnt,
                    "pct": pct,
                    "url": f"{PUBLIC_BASE_URL}/study/plan/{row['token']}",
                    "local_url": f"http://127.0.0.1:18081/study/plan/{row['token']}",
                    "image_base64": img_b64,
                })

            if path == "/internal/api/quiz/create":
                group_id = str(body.get("group_id") or "default")
                owner_name = str(body.get("owner_name") or "")
                subject = str(body.get("subject") or "综合测验")
                title = str(body.get("title") or f"{subject}小测验")
                difficulty = str(body.get("difficulty") or "适中")
                raw_qs = body.get("questions") or []
                norm_qs = []
                for q in raw_qs[:15]:
                    if not isinstance(q, dict):
                        continue
                    opts = [str(o) for o in (q.get("options") or [])[:4]]
                    if len(opts) < 2:
                        continue
                    ans = int(q.get("answer", 0))
                    if ans < 0 or ans >= len(opts):
                        ans = 0
                    norm_qs.append({
                        "question": str(q.get("question") or ""),
                        "options": opts,
                        "answer": ans,
                        "explanation": str(q.get("explanation") or ""),
                        "hint": str(q.get("hint") or ""),
                        "tag": str(q.get("tag") or subject),
                    })
                if not norm_qs:
                    return self._send_json(400, {"ok": False, "error": "questions_empty"})
                token = secrets.token_urlsafe(9)
                now = int(time.time())
                conn = get_conn()
                conn.execute(
                    "INSERT INTO study_quizzes (token, group_id, owner_name, subject, title, difficulty, questions_json, answers_json, score, total, submitted_at, created_at) VALUES (?,?,?,?,?,?,?,?,?,?,?,?)",
                    (token, group_id, owner_name, subject, title, difficulty, json.dumps(norm_qs, ensure_ascii=False), "{}", 0, len(norm_qs), 0, now),
                )
                conn.commit()
                conn.close()
                img_b64 = render_quiz_card_b64(title, subject, difficulty, norm_qs)
                return self._send_json(200, {
                    "ok": True,
                    "token": token,
                    "total": len(norm_qs),
                    "url": f"{PUBLIC_BASE_URL}/study/quiz/{token}",
                    "local_url": f"http://127.0.0.1:18081/study/quiz/{token}",
                    "image_base64": img_b64,
                })

            if path == "/internal/api/quiz/latest":
                group_id = str(body.get("group_id") or "default")
                conn = get_conn()
                row = conn.execute(
                    "SELECT * FROM study_quizzes WHERE group_id = ? ORDER BY created_at DESC LIMIT 1",
                    (group_id,),
                ).fetchone()
                conn.close()
                if not row:
                    return self._send_json(200, {"ok": True, "found": False})
                qs = json.loads(row["questions_json"])
                ans = json.loads(row["answers_json"] or "{}")
                wrong_details = []
                for idx, q in enumerate(qs):
                    ch = ans.get(str(idx))
                    if ch is not None and ch != q["answer"]:
                        wrong_details.append({
                            "index": idx + 1,
                            "question": q["question"],
                            "chosen": q["options"][ch] if 0 <= ch < len(q["options"]) else str(ch),
                            "correct": q["options"][q["answer"]],
                            "tag": q.get("tag", ""),
                        })
                return self._send_json(200, {
                    "ok": True,
                    "found": True,
                    "token": row["token"],
                    "title": row["title"],
                    "subject": row["subject"],
                    "answered_count": len(ans),
                    "score": row["score"],
                    "total": row["total"],
                    "wrong_details": wrong_details,
                    "url": f"{PUBLIC_BASE_URL}/study/quiz/{row['token']}",
                    "local_url": f"http://127.0.0.1:18081/study/quiz/{row['token']}",
                })

        return self._send_json(404, {"ok": False, "error": "not_found"})


def main():
    init_db()
    server = ThreadingHTTPServer((LISTEN_HOST, LISTEN_PORT), StudyHubHandler)
    print(f"[StudyHub] 服务已启动: http://{LISTEN_HOST}:{LISTEN_PORT} (字体: {REGULAR_FONT_PATH})", flush=True)
    server.serve_forever()


if __name__ == "__main__":
    main()
