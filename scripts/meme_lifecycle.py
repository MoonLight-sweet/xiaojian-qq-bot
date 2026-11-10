#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""
新梗自动学习与生命周期迭代系统 (Meme Lifecycle Manager)
功能：
1. 定时拉取最新流行热榜与趋势话题；
2. 过滤违规与低俗内容，提取符合黑猫人设的幽默接梗素材；
3. 将新梗录入长期记忆库，并为每个梗设定时效周期；
4. 自动扫描并归档（淘汰）超过时效期的陈旧老梗。
"""

import urllib.request
import json
import sqlite3
import time
import os
import sys

DB_PATH = "/opt/gemini-qq-bot/data/bot.sqlite3"
BOT_ID = "3644193154"
GROUP_ID = "1077671339"

# 梗的活跃周期：14天（秒）
MEME_TTL_SECONDS = 14 * 24 * 3600

# 屏蔽词列表：避免政治、低俗、恶性社会事件污染人设
BLOCK_KEYWORDS = [
    "案", "死", "伤", "抓获", "审判", "官员", "贪腐", "涉案",
    "出轨", "自杀", "坠楼", "毒品", "违纪", "政协", "党委"
]

def fetch_trending_topics():
    """从开放热点接口获取当前实时趋势与新梗"""
    url = "https://api.bilibili.com/x/web-interface/wbi/search/square?limit=30"
    headers = {
        "User-Agent": "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36"
    }
    req = urllib.request.Request(url, headers=headers)
    try:
        with urllib.request.urlopen(req, timeout=10) as resp:
            data = json.loads(resp.read().decode('utf-8'))
            if data.get("code") == 0:
                items = data.get("data", {}).get("trending", {}).get("list", [])
                topics = []
                for item in items:
                    kw = item.get("keyword", "").strip()
                    if kw and len(kw) >= 2:
                        if not any(bk in kw for bk in BLOCK_KEYWORDS):
                            topics.append(kw)
                return topics
    except Exception as e:
        print(f"[WARN] 获取热搜数据失败: {e}")
    return []

def update_memes_and_evict_old():
    if not os.path.exists(DB_PATH):
        print(f"[ERROR] 数据库不存在: {DB_PATH}")
        return

    now = int(time.time())
    conn = sqlite3.connect(DB_PATH)
    cursor = conn.cursor()

    # 1. 淘汰过气老梗：将超过 TTL 且未归档的 meme 标记归档
    expire_cutoff = now - MEME_TTL_SECONDS
    cursor.execute("""
        UPDATE memories 
        SET archived_at = ?, updated_at = ?
        WHERE kind = 'meme' AND created_at < ? AND archived_at IS NULL
    """, (now, now, expire_cutoff))
    evicted_count = cursor.rowcount
    if evicted_count > 0:
        print(f"[EVICT] 已成功淘汰并归档 {evicted_count} 个过气老梗！")

    # 2. 抓取新热点
    trending = fetch_trending_topics()
    print(f"[FETCH] 获取到 {len(trending)} 条新鲜热点话题。")

    inserted = 0
    # 选取前 8 个精选潮流梗录入
    for topic in trending[:8]:
        ref = f"meme_{int(time.time()*1000)%100000000}_{inserted}"
        content = f"网络新鲜热点与流行梗（当下活跃流行）：{topic}。可作为聊天打趣、调侃或接梗素材，遇到群友讨论可自然接话。"
        
        # 检查是否已有类似活跃梗
        cursor.execute("SELECT memory_ref FROM memories WHERE kind = 'meme' AND content LIKE ? AND archived_at IS NULL", (f"%{topic}%",))
        if cursor.fetchone() is not None:
            continue

        cursor.execute("""
            INSERT INTO memories (
                memory_ref, bot_id, group_id, subject_user_id,
                content, kind, source_message_ids, confidence,
                revision, created_at, updated_at, origin, archived_at
            ) VALUES (?, ?, ?, NULL, ?, 'meme', '[]', 0.9, 1, ?, ?, 'trending_sync', NULL)
        """, (ref, BOT_ID, GROUP_ID, content, now, now))
        inserted += 1

    conn.commit()
    conn.close()
    print(f"[SUCCESS] 成功入库 {inserted} 个最新活跃流行梗！")

if __name__ == '__main__':
    update_memes_and_evict_old()
