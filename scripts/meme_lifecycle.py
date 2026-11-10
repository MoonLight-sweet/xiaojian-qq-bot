#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""
新梗与游戏社区热度自动学习与生命周期迭代系统 (Meme & Heybox Gaming Lifecycle Manager)
功能：
1. 定时拉取全网流行热点趋势（B站、短视频等综合热榜）；
2. 抓取小黑盒/核心游戏社区热议话题、Steam喜加一与玩家黑话；
3. 过滤无关违规内容，提取适合黑猫傲娇俏皮人设打趣、调侃的接地气素材；
4. 将新梗录入长期记忆库（memories 表），设定 14 天生命周期时效；
5. 自动扫描并归档（淘汰）过气老梗，保持机器人的梗库始终处于当下最新鲜活跃状态。
"""

import urllib.request
import json
import sqlite3
import time
import os
import sys
import re

DB_PATH = "/opt/gemini-qq-bot/data/bot.sqlite3"
BOT_ID = "3644193154"
GROUP_ID = "1077671339"

# 梗的活跃周期：14天（秒）
MEME_TTL_SECONDS = 14 * 24 * 3600

# 屏蔽词列表：避免政治、低俗、恶性社会事件污染人设
BLOCK_KEYWORDS = [
    "案", "死", "伤", "抓获", "审判", "官员", "贪腐", "涉案",
    "出轨", "自杀", "坠楼", "毒品", "违纪", "政协", "党委", "命案"
]

# 小黑盒与核心玩家高频黑话与生态知识库（动态轮换录入，带14天时效）
HEYBOX_GAMING_MEMES = [
    {
        "topic": "喜加一与Steam打折",
        "desc": "Steam大促、史低折扣或Epic喜加一白嫖游戏。玩家日常调侃：买游戏不玩叫喜加一，买了放库里也是玩，省下的就是赚到的。"
    },
    {
        "topic": "电子阳痿",
        "desc": "形容买了海量3A大作却完全提不起劲打开，坐在电脑前发呆或者只能玩得下小品级独立游戏和刷短视频的玩家精神状态。"
    },
    {
        "topic": "受苦与打铁",
        "desc": "魂系游戏（法环、黑神话、只狼）玩家日常，被BOSS虐千百遍依然乐在其中，把弹刀格挡音效戏称为愉悦的打铁声。"
    },
    {
        "topic": "开香槟与半场开香槟",
        "desc": "电竞比赛或攻克难关时局势刚有点优势就提前庆祝，结果惨遭反转被翻盘打脸，常用于互相调侃别高兴太早。"
    },
    {
        "topic": "红温破防与急了",
        "desc": "对局被搞心态、排位连跪或者打BOSS被残血反杀时当场血压升高、脸色通红破防，是游戏群里高频斗嘴用语。"
    },
    {
        "topic": "包爽的 / 太美丽了",
        "desc": "游戏群友安利硬核游戏或魔性玩法时的诱导口头禅，表面保证体验极佳，实际可能是推群友进受苦大坑。"
    },
    {
        "topic": "坐牢与牛马排位",
        "desc": "遇到坑队友连输几把掉分，如同在游戏里服刑坐牢；常用于下播吐槽或劝群友早点收手去睡觉。"
    },
    {
        "topic": "背刺与早买早享受",
        "desc": "刚原价入手游戏不久官方就突击进包或骨折平史低，玩家常哀嚎被背刺，群友则回复早买早享受晚买享折扣不买免费送。"
    }
]

def fetch_trending_topics():
    """从开放热点接口获取当前实时趋势与新梗"""
    url = "https://api.bilibili.com/x/web-interface/wbi/search/square?limit=30"
    headers = {
        "User-Agent": "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36"
    }
    req = urllib.request.Request(url, headers=headers)
    topics = []
    try:
        with urllib.request.urlopen(req, timeout=10) as resp:
            data = json.loads(resp.read().decode('utf-8'))
            if data.get("code") == 0:
                items = data.get("data", {}).get("trending", {}).get("list", [])
                for item in items:
                    kw = item.get("keyword", "").strip()
                    if kw and len(kw) >= 2:
                        if not any(bk in kw for bk in BLOCK_KEYWORDS):
                            topics.append(kw)
    except Exception as e:
        print(f"[WARN] 获取全网热搜失败: {e}")
    return topics

def fetch_gaming_hot_topics():
    """从游戏区热度榜获取当前最新游戏热议话题（小黑盒/核心玩家热点）"""
    url = "https://api.bilibili.com/x/web-interface/ranking/v2?rid=4&type=all"
    headers = {
        "User-Agent": "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36"
    }
    req = urllib.request.Request(url, headers=headers)
    gaming_topics = []
    try:
        with urllib.request.urlopen(req, timeout=10) as resp:
            data = json.loads(resp.read().decode('utf-8'))
            if data.get("code") == 0:
                items = data.get("data", {}).get("list", [])
                for item in items[:15]:
                    title = item.get("title", "").strip()
                    # 清洗标题特殊符号
                    clean_title = re.sub(r'【.*?】|\[.*?\]', '', title).strip()
                    if clean_title and len(clean_title) >= 4:
                        if not any(bk in clean_title for bk in BLOCK_KEYWORDS):
                            gaming_topics.append(clean_title)
    except Exception as e:
        print(f"[WARN] 获取游戏热点榜失败: {e}")
    return gaming_topics

def update_memes_and_evict_old():
    if not os.path.exists(DB_PATH):
        print(f"[ERROR] 数据库不存在: {DB_PATH}")
        return

    now = int(time.time())
    conn = sqlite3.connect(DB_PATH)
    cursor = conn.cursor()

    # 1. 淘汰过气老梗：将超过 14 天 TTL 且未归档的 meme 自动标记归档
    expire_cutoff = now - MEME_TTL_SECONDS
    cursor.execute("""
        UPDATE memories 
        SET archived_at = ?, updated_at = ?
        WHERE kind = 'meme' AND created_at < ? AND archived_at IS NULL
    """, (now, now, expire_cutoff))
    evicted_count = cursor.rowcount
    if evicted_count > 0:
        print(f"[EVICT] 已成功淘汰并归档 {evicted_count} 个过气老梗！")

    inserted = 0

    # 2. 抓取全网新热点趋势（前 6 个精选）
    trending = fetch_trending_topics()
    print(f"[FETCH] 获取到 {len(trending)} 条全网新鲜热搜话题。")
    for topic in trending[:6]:
        ref = f"meme_{int(time.time()*1000)%100000000}_{inserted}"
        content = f"网络新鲜热点与流行梗（当下活跃流行）：{topic}。可作为聊天打趣、调侃或接梗素材，遇到群友讨论可自然接话。"
        
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

    # 3. 抓取小黑盒/核心游戏社区热议话题（前 4 个精选）
    gaming_hot = fetch_gaming_hot_topics()
    print(f"[FETCH] 获取到 {len(gaming_hot)} 条核心游戏区热度榜话题。")
    for gtopic in gaming_hot[:4]:
        ref = f"meme_game_{int(time.time()*1000)%100000000}_{inserted}"
        content = f"小黑盒/游戏圈当下热议话题：{gtopic}。适合在群友聊游戏、Steam、通关、攻略或吐槽游戏体验时作为内行玩家话题自然切入。"

        cursor.execute("SELECT memory_ref FROM memories WHERE kind = 'meme' AND content LIKE ? AND archived_at IS NULL", (f"%{gtopic[:15]}%",))
        if cursor.fetchone() is not None:
            continue

        cursor.execute("""
            INSERT INTO memories (
                memory_ref, bot_id, group_id, subject_user_id,
                content, kind, source_message_ids, confidence,
                revision, created_at, updated_at, origin, archived_at
            ) VALUES (?, ?, ?, NULL, ?, 'meme', '[]', 0.9, 1, ?, ?, 'heybox_sync', NULL)
        """, (ref, BOT_ID, GROUP_ID, content, now, now))
        inserted += 1

    # 4. 补充小黑盒常驻玩家黑话与吐槽（轮换选取 3 个未入库或已过期的梗激活）
    for item in HEYBOX_GAMING_MEMES:
        t = item["topic"]
        d = item["desc"]
        cursor.execute("SELECT memory_ref FROM memories WHERE kind = 'meme' AND content LIKE ? AND archived_at IS NULL", (f"%{t}%",))
        if cursor.fetchone() is not None:
            continue

        ref = f"meme_lingo_{int(time.time()*1000)%100000000}_{inserted}"
        content = f"小黑盒/玩家圈黑话与游戏梗：【{t}】。{d} 群友聊游戏、抽卡、受苦或自嘲时可自然接话调侃。"
        cursor.execute("""
            INSERT INTO memories (
                memory_ref, bot_id, group_id, subject_user_id,
                content, kind, source_message_ids, confidence,
                revision, created_at, updated_at, origin, archived_at
            ) VALUES (?, ?, ?, NULL, ?, 'meme', '[]', 0.95, 1, ?, ?, 'heybox_lingo', NULL)
        """, (ref, BOT_ID, GROUP_ID, content, now, now))
        inserted += 1
        if inserted >= 12:
            break

    conn.commit()
    conn.close()
    print(f"[SUCCESS] 流程完成！当前新录入 {inserted} 个热梗与小黑盒游戏社区知识！")

if __name__ == '__main__':
    update_memes_and_evict_old()
