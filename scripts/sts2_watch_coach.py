#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""
小简《杀戮尖塔2》师徒观战学徒模式 v2 (Spectator Coach & Interactive Q&A Overlay)
修复与升级：
1. 后台独立线程读取 %TEMP%/sts2-ai-agent-instance-*.json 动态发现真实端口并轮询快照，绝不卡死界面；
2. 弹窗出现时持续调用 Win32 SetWindowPos(HWND_TOPMOST) 强制置顶，确保全屏游戏下依然清晰可见；
3. 拓宽关键操作触发：新增地图分叉选路、牌组新增/删除卡牌、喝药水、获得新遗物/海克斯、火堆决策、战斗掉血或击杀；
4. 启动后立即在右下角展示 6 秒就位提示卡片，确认弹窗可见；全流程记录日志至 data/sts2_watch_coach.log。
"""

import ctypes
import glob
import json
import os
import queue
import tempfile
import threading
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
LOG_FILE = os.path.join(BASE_DIR, "data", "sts2_watch_coach.log")


def log_msg(msg):
    try:
        os.makedirs(os.path.dirname(LOG_FILE), exist_ok=True)
        with open(LOG_FILE, "a", encoding="utf-8") as f:
            f.write(f"[{time.strftime('%H:%M:%S')}] {msg}\n")
    except Exception:
        pass


def discover_ports():
    ports = []
    # 1. 优先读取 STS2AIAgent 写入临时目录的实例发现文件
    try:
        tmp = tempfile.gettempdir()
        for fp in glob.glob(os.path.join(tmp, "sts2-ai-agent-instance-*.json")):
            try:
                with open(fp, "r", encoding="utf-8") as f:
                    d = json.load(f)
                p = int(d.get("api_port") or d.get("port") or 0)
                if p and p not in ports:
                    ports.append(p)
            except Exception:
                pass
    except Exception:
        pass
    # 2. 常见端口兜底
    for p in (8081, 8080, 8082, 8083, 8084, 8085):
        if p not in ports:
            ports.append(p)
    return ports


def fetch_snapshot(cached_port=None):
    ports = ([cached_port] if cached_port else []) + [p for p in discover_ports() if p != cached_port]
    for port in ports:
        try:
            with urllib.request.urlopen(f"http://127.0.0.1:{port}/decision-snapshot", timeout=0.8) as r:
                if r.status == 200:
                    raw = json.loads(r.read().decode("utf-8", "ignore"))
                    if isinstance(raw, dict) and (raw.get("ok") is True or "state" in raw or "data" in raw):
                        data = raw.get("data") if isinstance(raw.get("data"), dict) else raw
                        return port, data
        except Exception:
            continue
    return None, None


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
    log_msg(f"SAVED TEACHING [{category}]: {answer.strip()}")


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


def _card_name(c):
    if isinstance(c, str):
        return c
    if isinstance(c, dict):
        return str(c.get("name") or c.get("line") or c.get("card_id") or "新牌").split("：")[0]
    return "新牌"


def detect_notable_player_action(prev_snap, cur_snap):
    ps = prev_snap.get("state") or {}
    cs = cur_snap.get("state") or {}
    p_screen = str(ps.get("screen") or "").upper()
    c_screen = str(cs.get("screen") or "").upper()
    p_run = ps.get("run") or {}
    c_run = cs.get("run") or {}
    floor = c_run.get("floor", "?")

    # 1. 喝药水
    p_pots = [p.get("name") or p.get("potion_id") for p in (p_run.get("potions") or []) if isinstance(p, dict) and (p.get("potion_id") or p.get("occupied"))]
    c_pots = [p.get("name") or p.get("potion_id") for p in (c_run.get("potions") or []) if isinstance(p, dict) and (p.get("potion_id") or p.get("occupied"))]
    if len(p_pots) > len(c_pots):
        used = [x for x in p_pots if x not in c_pots] or [p_pots[0]]
        return (
            "药水时机",
            f"第{floor}层使用药水 {used[0]}",
            f"师父，我看你刚在第{floor}层喝了【{used[0]}】！这个时机喝药水主要是为了保血、抢斩杀还是腾药水格子呀？",
        )

    # 2. 获得新的海克斯符文或遗物
    p_rels = list(p_run.get("relics") or [])
    c_rels = list(c_run.get("relics") or [])
    if len(c_rels) > len(p_rels) and len(p_rels) > 0:
        new_r = c_rels[-1]
        rname = new_r if isinstance(new_r, str) else (new_r.get("name") or new_r.get("relic_id") or "新遗物")
        return (
            "遗物/海克斯联动",
            f"新获得遗物或海克斯符文 {rname}",
            f"你刚刚拿了【{rname}】！实战里拿到它之后，出牌顺序或后续抓牌应该怎么配合它呀？",
        )

    # 3. 牌组新增或删除了卡牌（无论从奖励、商店、事件还是火堆添柴）
    p_deck = p_run.get("deck") or []
    c_deck = c_run.get("deck") or []
    if len(p_deck) > 0 and len(c_deck) > len(p_deck):
        added = _card_name(c_deck[-1])
        return (
            "抓牌思路",
            f"第{floor}层牌组新增卡牌 {added}",
            f"你刚把【{added}】加入牌组啦！能教教我这张牌主要是补伤害、补防御还是配合当前流派吗？",
        )
    if len(p_deck) > 0 and len(c_deck) < len(p_deck):
        return (
            "删牌精简",
            f"第{floor}层删除了牌组中的卡牌",
            f"你刚才删掉了一张牌！精简卡组时你一般优先删基础打击还是防御呀？",
        )

    # 4. 跳过选牌奖励
    if p_screen == "REWARD" and c_screen != "REWARD" and (ps.get("reward") or {}).get("pending_card_choice"):
        return (
            "跳过选牌",
            f"第{floor}层跳过选牌奖励",
            f"刚才那三张奖励牌你直接跳过了，是不是因为都不契合流派、怕稀释牌组抽牌呀？",
        )

    # 5. 火堆休息/升级
    if p_screen == "REST" and c_screen != "REST":
        php, pmax = _parse_hp(p_run.get("hp"))
        chp, _ = _parse_hp(c_run.get("hp"))
        if chp > php:
            return (
                "火堆回血",
                f"火堆选择休息回血 ({php}->{chp}/{pmax})",
                f"刚才在火堆（血量{php}/{pmax}）你选择了回血，一般掉到多少血以下你才会放弃升级选回血呀？",
            )
        return (
            "火堆锻造",
            f"火堆选择升级卡牌 (血量{php}/{pmax})",
            f"刚才血量{php}/{pmax}你选了敲铁匠升级，这种时候优先升级哪类牌收益最高呀？",
        )

    # 6. 地图分叉选路进入下一房间
    if p_screen == "MAP" and c_screen in ("COMBAT", "EVENT", "SHOP", "REST", "CHEST"):
        room_cn = {"COMBAT": "战斗房", "EVENT": "问号事件", "SHOP": "商店", "REST": "火堆", "CHEST": "宝箱"}.get(c_screen, c_screen)
        hp, max_hp = _parse_hp(c_run.get("hp"))
        return (
            "路线规划",
            f"第{floor}层地图选择进入{room_cn} (血量{hp}/{max_hp})",
            f"刚才地图分叉你选了进【{room_cn}】（当前血量{hp}/{max_hp}），选这条路线是怎么考虑的呀？",
        )

    # 7. 战斗中刚结束一回合且掉血 >= 6 或刚击杀一只怪物
    if p_screen == "COMBAT" and c_screen == "COMBAT":
        pt = ps.get("turn")
        ct = cs.get("turn")
        if pt is not None and ct is not None and ct > pt:
            php, pmax = _parse_hp(p_run.get("hp"))
            chp, _ = _parse_hp(c_run.get("hp"))
            lost = php - chp
            if lost >= 6:
                return (
                    "卖血与攻防取舍",
                    f"第{floor}层第{pt}回合承受{lost}点伤害",
                    f"上一回合你吃了{lost}点伤害，是因为手牌防御不够，还是故意卖血抢输出/留节奏呀？",
                )

    return None


class SpectatorCoachApp:
    def __init__(self):
        self.q = queue.Queue()
        self.pending_q = None
        self.popup_shown_at = 0.0
        self.auto_hide_sec = 30.0
        self.stop_event = threading.Event()

        self.root = tk.Tk()
        self.root.title("XiaojianSTS2WatchCoach")
        self.root.overrideredirect(True)
        self.root.attributes("-topmost", True)
        self.root.attributes("-alpha", 0.94)
        self.root.configure(bg="#0F172A")

        sw = self.root.winfo_screenwidth()
        sh = self.root.winfo_screenheight()
        self.w, self.h = 460, 200
        self.x = max(12, sw - self.w - 22)
        self.y = max(12, sh - self.h - 70)
        self.root.geometry(f"{self.w}x{self.h}+{self.x}+{self.y}")

        frame = tk.Frame(self.root, bg="#0F172A", highlightbackground="#38BDF8", highlightthickness=2)
        frame.pack(fill="both", expand=True)

        top = tk.Frame(frame, bg="#1E293B")
        top.pack(fill="x")
        self.lbl_tag = tk.Label(
            top,
            text="● 小简观战学徒 · 已就位",
            fg="#38BDF8",
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

        self.lbl_q = tk.Label(
            frame,
            text="师徒观战模式已启动！你切到《杀戮尖塔2》正常打牌即可，遇到选牌、喝药水、走路线或关键操作时我会在这里弹出向你请教。",
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

        bottom = tk.Frame(frame, bg="#0F172A", padx=10, pady=8)
        bottom.pack(fill="x")
        self.entry = tk.Entry(
            bottom,
            bg="#1E293B",
            fg="#FFFFFF",
            insertbackground="#38BDF8",
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

        self.root.update_idletasks()
        self.hwnd = ctypes.windll.user32.GetParent(self.root.winfo_id()) or self.root.winfo_id()

        # 启动时展示 6 秒就位提示，让用户立刻亲眼确认右下角弹窗有效
        self.popup_shown_at = time.time()
        self.auto_hide_sec = 6.0
        self._force_topmost()

        threading.Thread(target=self.bg_watcher_loop, daemon=True).start()
        self.root.after(250, self.ui_tick)

    def _force_topmost(self):
        try:
            # HWND_TOPMOST=-1, SWP_NOACTIVATE=0x0010, SWP_SHOWWINDOW=0x0040
            ctypes.windll.user32.SetWindowPos(self.hwnd, -1, self.x, self.y, self.w, self.h, 0x0010 | 0x0040)
        except Exception:
            pass

    def show_question(self, category, ctx, question):
        self.pending_q = (category, ctx, question)
        self.lbl_tag.config(text=f"● 小简观战学徒 · {category}")
        self.lbl_q.config(text=question)
        self.entry.delete(0, tk.END)
        self.root.deiconify()
        self._force_topmost()
        self.popup_shown_at = time.time()
        self.auto_hide_sec = 32.0
        log_msg(f"POPUP SHOWN [{category}]: {question}")

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
        self.popup_shown_at = time.time()
        self.auto_hide_sec = 2.0

    def bg_watcher_loop(self):
        log_msg("WATCHER THREAD STARTED")
        prev_snap = None
        cached_port = None
        last_ask = 0.0
        connected_logged = False
        while not self.stop_event.is_set():
            time.sleep(0.8)
            if not is_watch_enabled():
                self.q.put(("QUIT", None, None))
                return
            port, snap = fetch_snapshot(cached_port)
            if port:
                cached_port = port
                if not connected_logged:
                    log_msg(f"CONNECTED TO STS2AIAgent on port {port}")
                    connected_logged = True
            else:
                if connected_logged:
                    log_msg("DISCONNECTED FROM STS2AIAgent")
                    connected_logged = False
                cached_port = None
                continue

            if snap and prev_snap:
                if (time.time() - last_ask) >= 20.0:
                    found = detect_notable_player_action(prev_snap, snap)
                    if found:
                        last_ask = time.time()
                        self.q.put(found)
            if snap:
                prev_snap = snap

    def ui_tick(self):
        if not is_watch_enabled():
            self.stop_event.set()
            self.root.destroy()
            return

        try:
            while True:
                cat, ctx, q_txt = self.q.get_nowait()
                if cat == "QUIT":
                    self.stop_event.set()
                    self.root.destroy()
                    return
                if not self.pending_q:
                    self.show_question(cat, ctx, q_txt)
        except queue.Empty:
            pass

        if self.root.state() != "withdrawn":
            self._force_topmost()
            if (time.time() - self.popup_shown_at) > self.auto_hide_sec and not self.entry.get().strip():
                self.hide_popup()

        self.root.after(250, self.ui_tick)

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
