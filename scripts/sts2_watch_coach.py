#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""
小简《杀戮尖塔2》师徒观战学徒模式 (Spectator Coach & Interactive Q&A Overlay)
功能：
1. 只观战、绝不抢夺玩家鼠标和键盘，通过 STS2AIAgent 每 0.8 秒静默监听玩家操作；
2. 当检测到高价值决策（喝药水、选/跳过奖励卡、走精英/火堆/商店、触发海克斯符文、放弃叠甲抢伤害、删牌/升级等）时，
   在游戏屏幕右下角弹出一个小巧不挡牌的置顶提问卡片，询问“为什么这么做”；
3. 玩家随手敲一句思路按回车，立刻写入 data/sts2_knowledge/sts2_lessons_learned.json 的 mentor_teachings，
   以后机器人自己游玩和决策时会自动学习复用这些实战思路；点“跳过”或 25 秒无操作自动收起，绝不打扰打牌。
"""

import ctypes
import json
import os
import time
import tkinter as tk
import urllib.request

try:
    ctypes.windll.user32.SetProcessDPIAware()
except Exception:
    pass

BASE_DIR = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
LESSONS_FILE = os.path.join(BASE_DIR, "data", "sts2_knowledge", "sts2_lessons_learned.json")
WATCH_STATE_FILE = os.path.join(BASE_DIR, "data", "sts2_watch_state.json")
PID_FILE = os.path.join(BASE_DIR, "data", "sts2_watch_coach.pid")


def fetch_snapshot():
    for port in (8081, 8080, 8082):
        try:
            with urllib.request.urlopen(f"http://127.0.0.1:{port}/decision-snapshot", timeout=1.5) as r:
                if r.status == 200:
                    raw = json.loads(r.read().decode("utf-8", "ignore"))
                    return raw.get("data") if isinstance(raw, dict) and isinstance(raw.get("data"), dict) else raw
        except Exception:
            continue
    return None


def save_mentor_teaching(category, context_desc, question, answer):
    os.makedirs(os.path.dirname(LESSONS_FILE), exist_ok=True)
    data = {
        "version": 1,
        "global_iron_rules": [],
        "enemy_specific_lessons": {},
        "run_reviews": [],
        "mentor_teachings": [],
    }
    if os.path.exists(LESSONS_FILE):
        try:
            with open(LESSONS_FILE, "r", encoding="utf-8") as f:
                data = json.load(f)
        except Exception:
            pass
    teachings = data.setdefault("mentor_teachings", [])
    now_str = time.strftime("%Y-%m-%d %H:%M:%S")
    teachings.append({
        "date": now_str,
        "category": category,
        "context": context_desc,
        "question": question,
        "player_reasoning": answer.strip(),
    })
    data["updated_at"] = now_str
    with open(LESSONS_FILE, "w", encoding="utf-8") as f:
        json.dump(data, f, ensure_ascii=False, indent=2)


def is_watch_enabled():
    if not os.path.exists(WATCH_STATE_FILE):
        return True
    try:
        with open(WATCH_STATE_FILE, "r", encoding="utf-8") as f:
            st = json.load(f)
        return bool(st.get("active", True))
    except Exception:
        return True


def _parse_hp(hp_str):
    if isinstance(hp_str, str) and "/" in hp_str:
        try:
            a, b = hp_str.split("/", 1)
            return int(a.strip()), int(b.strip())
        except Exception:
            pass
    return 0, 0


class SpectatorCoachApp:
    def __init__(self):
        self.prev_snap = None
        self.last_ask_time = 0.0
        self.cooldown_sec = 22.0  # 两次主动提问之间至少间隔 22 秒，不打扰节奏
        self.pending_q = None     # (category, context, question)
        self.popup_shown_at = 0.0

        self.root = tk.Tk()
        self.root.title("XiaojianSTS2WatchCoach")
        self.root.overrideredirect(True)
        self.root.attributes("-topmost", True)
        self.root.attributes("-alpha", 0.94)
        self.root.configure(bg="#0F172A")

        sw = self.root.winfo_screenwidth()
        sh = self.root.winfo_screenheight()
        self.w, self.h = 460, 195
        self.x = max(12, sw - self.w - 22)
        self.y = max(12, sh - self.h - 70)
        self.root.geometry(f"{self.w}x{self.h}+{self.x}+{self.y}")

        frame = tk.Frame(self.root, bg="#0F172A", highlightbackground="#3B82F6", highlightthickness=2)
        frame.pack(fill="both", expand=True)

        # 标题栏
        top = tk.Frame(frame, bg="#1E293B")
        top.pack(fill="x")
        self.lbl_tag = tk.Label(
            top,
            text="● 小简观战学徒 · 请教思路",
            fg="#60A5FA",
            bg="#1E293B",
            font=("Microsoft YaHei UI", 10, "bold"),
            padx=10,
            pady=6,
            anchor="w",
        )
        self.lbl_tag.pack(side="left", fill="x", expand=True)

        btn_close = tk.Button(
            top,
            text="跳过",
            command=self.hide_popup,
            bg="#334155",
            fg="#E2E8F0",
            relief="flat",
            font=("Microsoft YaHei UI", 9),
            padx=8,
            pady=2,
            cursor="hand2",
        )
        btn_close.pack(side="right", padx=8, pady=4)

        # 问题内容
        self.lbl_q = tk.Label(
            frame,
            text="",
            fg="#F8FAFC",
            bg="#0F172A",
            font=("Microsoft YaHei UI", 10),
            wraplength=430,
            justify="left",
            anchor="nw",
            padx=12,
            pady=8,
        )
        self.lbl_q.pack(fill="both", expand=True)

        # 输入框与提交按钮
        bottom = tk.Frame(frame, bg="#0F172A", padx=10, pady=8)
        bottom.pack(fill="x")
        self.entry = tk.Entry(
            bottom,
            bg="#1E293B",
            fg="#FFFFFF",
            insertbackground="#60A5FA",
            relief="flat",
            font=("Microsoft YaHei UI", 10),
        )
        self.entry.pack(side="left", fill="x", expand=True, ipady=5, padx=(0, 8))
        self.entry.bind("<Return>", lambda _e: self.submit_answer())
        self.entry.bind("<Escape>", lambda _e: self.hide_popup())

        self.btn_send = tk.Button(
            bottom,
            text="教给小简",
            command=self.submit_answer,
            bg="#2563EB",
            fg="#FFFFFF",
            relief="flat",
            font=("Microsoft YaHei UI", 9, "bold"),
            padx=12,
            pady=5,
            cursor="hand2",
        )
        self.btn_send.pack(side="right")

        self.root.withdraw()
        self.root.after(800, self.poll_game)

    def show_question(self, category, ctx, question):
        self.pending_q = (category, ctx, question)
        self.lbl_tag.config(text=f"● 小简观战学徒 · {category}")
        self.lbl_q.config(text=question)
        self.entry.delete(0, tk.END)
        self.root.deiconify()
        self.root.lift()
        self.popup_shown_at = time.time()
        self.last_ask_time = time.time()

    def hide_popup(self):
        self.pending_q = None
        self.root.withdraw()

    def submit_answer(self):
        ans = self.entry.get().strip()
        if not ans or not self.pending_q:
            self.hide_popup()
            return
        cat, ctx, q = self.pending_q
        save_mentor_teaching(cat, ctx, q, ans)
        self.lbl_tag.config(text="● 已学会！记入小简实战经验库")
        self.lbl_q.config(text=f"记下来啦：「{ans[:60]}」\n下次我遇到类似局面也会这么思考！")
        self.entry.delete(0, tk.END)
        self.pending_q = None
        self.root.after(1500, self.hide_popup)

    def detect_notable_player_action(self, prev_snap, cur_snap):
        ps = prev_snap.get("state") or {}
        cs = cur_snap.get("state") or {}
        p_screen = str(ps.get("screen") or "").upper()
        c_screen = str(cs.get("screen") or "").upper()
        p_run = ps.get("run") or {}
        c_run = cs.get("run") or {}

        # 1. 检测到玩家刚刚喝了药水
        p_pots = [p.get("name") or p.get("potion_id") for p in (p_run.get("potions") or []) if p.get("potion_id") or p.get("occupied")]
        c_pots = [p.get("name") or p.get("potion_id") for p in (c_run.get("potions") or []) if p.get("potion_id") or p.get("occupied")]
        if len(p_pots) > len(c_pots):
            used = [x for x in p_pots if x not in c_pots] or [p_pots[0]]
            floor = c_run.get("floor", "?")
            return (
                "药水时机",
                f"第{floor}层使用药水 {used[0]}",
                f"师父，我看你刚在第{floor}层喝了【{used[0]}】！这个时机喝药水主要是为了保血、抢斩杀还是腾药水格子呀？",
            )

        # 2. 检测到玩家刚刚获得了新的海克斯符文或遗物
        p_rels = list(p_run.get("relics") or [])
        c_rels = list(c_run.get("relics") or [])
        if len(c_rels) > len(p_rels):
            new_r = c_rels[-1]
            rname = new_r if isinstance(new_r, str) else (new_r.get("name") or new_r.get("relic_id") or "新遗物")
            return (
                "遗物/海克斯联动",
                f"新获得遗物或海克斯符文 {rname}",
                f"你刚刚拿了【{rname}】！实战里拿到它之后，出牌顺序或后续抓牌应该怎么配合它呀？",
            )

        # 3. 检测到玩家刚从选牌奖励离开（选了某张牌或直接跳过）
        p_deck = p_run.get("deck") or []
        c_deck = c_run.get("deck") or []
        if p_screen == "REWARD" and c_screen != "REWARD":
            if len(c_deck) > len(p_deck):
                added = c_deck[-1]
                cname = added if isinstance(added, str) else (added.get("name") or added.get("card_id") or "新牌")
                return (
                    "抓牌思路",
                    f"第{c_run.get('floor','?')}层抓取卡牌 {cname}",
                    f"刚才战利品选牌你拿了【{cname}】，能告诉我为什么选它不选另外两张吗？",
                )
            elif (ps.get("reward") or {}).get("pending_card_choice"):
                return (
                    "跳过选牌",
                    f"第{c_run.get('floor','?')}层跳过选牌奖励",
                    f"刚才那三张奖励牌你直接跳过了，是不是因为怕稀释牌组核心循环呀？",
                )

        # 4. 检测到玩家刚在火堆做了选择（回血/升级/添柴删牌）
        if p_screen == "REST" and c_screen != "REST":
            php, pmax = _parse_hp(p_run.get("hp"))
            chp, _ = _parse_hp(c_run.get("hp"))
            if chp > php:
                return (
                    "火堆决策",
                    f"火堆选择休息回血 ({php}->{chp}/{pmax})",
                    f"刚才在火堆（血量{php}/{pmax}）你选择了休息回血，通常掉到多少血以下你才会放弃升级选回血呀？",
                )
            elif len(c_deck) < len(p_deck):
                return (
                    "火堆添柴删牌",
                    f"火堆使用模组添柴删牌",
                    f"你刚在火堆用了海克斯模组的【添柴】删牌！一般优先删基础打击还是防御呀？",
                )
            else:
                return (
                    "火堆升级",
                    f"火堆锻造升级卡牌 (血量{php}/{pmax})",
                    f"刚才血量{php}/{pmax}你没回血而是选了敲铁匠升级，优先升级哪类牌收益最高呀？",
                )

        # 5. 检测到战斗中玩家刚结束回合，但本回合没叠满甲硬吃了伤害（抢输出/卖血决策）
        if p_screen == "COMBAT" and c_screen == "COMBAT":
            pt = ps.get("turn")
            ct = cs.get("turn")
            if pt is not None and ct is not None and ct > pt:
                php, pmax = _parse_hp(p_run.get("hp"))
                chp, _ = _parse_hp(c_run.get("hp"))
                lost = php - chp
                if lost >= 10:
                    return (
                        "卖血与攻防取舍",
                        f"第{c_run.get('floor','?')}层第{pt}回合承受{lost}点伤害",
                        f"上一回合你硬吃了{lost}点伤害，是因为手牌没防御，还是为了抢伤害/留费用做爆发呀？",
                    )

        return None

    def poll_game(self):
        if not is_watch_enabled():
            self.root.destroy()
            return

        # 弹窗超过 28 秒没输入自动收起，不遮挡视野
        if self.pending_q and (time.time() - self.popup_shown_at) > 28.0 and not self.entry.get().strip():
            self.hide_popup()

        snap = fetch_snapshot()
        if snap and self.prev_snap:
            if not self.pending_q and (time.time() - self.last_ask_time) >= self.cooldown_sec:
                found = self.detect_notable_player_action(self.prev_snap, snap)
                if found:
                    self.show_question(*found)
        if snap:
            self.prev_snap = snap

        self.root.after(800, self.poll_game)

    def run(self):
        os.makedirs(os.path.dirname(PID_FILE), exist_ok=True)
        try:
            with open(PID_FILE, "w", encoding="utf-8") as f:
                f.write(str(os.getpid()))
        except Exception:
            pass
        self.root.mainloop()


if __name__ == "__main__":
    SpectatorCoachApp().run()
