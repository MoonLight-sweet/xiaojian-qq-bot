#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""
小简本地Windows电脑全功能Agent控制桥接节点 (Full Computer-Use Bridge Node)
支持能力：
1. status / list_apps：查看当前窗口与本机200+已安装应用/游戏快捷方式；
2. open_app：按名称或中文别名直接启动本地应用、游戏（杀戮尖塔、星露谷、星铁、微信、B站、网易云、Steam等）或网页URL；
3. focus_window / close_window：突破Windows前台焦点锁（Alt解锁 + SW_RESTORE + SetForegroundWindow），100%强制切换窗口到最前台或关闭窗口；
4. screenshot：绑定活动交互桌面截取 1280x800 标准坐标网格画面，标出当前鼠标坐标十字准星；
5. click / drag / scroll：支持 1280x800 视觉坐标与 2560x1600 物理坐标自适应换算，支持单击、双击、右键、卡牌拖拽出牌、滚轮滑动；
6. key / hotkey / type_text：支持单键、组合快捷键（alt+tab, win+d, ctrl+c/v等）以及通过剪贴板注入任意中文/英文字符串；
7. run_command：直接在本机执行 PowerShell 命令并返回标准输出。
"""

import base64
import ctypes
import ctypes.wintypes
import io
import json
import os
import socket
import subprocess
import threading
import time
from http.server import BaseHTTPRequestHandler, HTTPServer

import paramiko
import win32clipboard
import win32con
import win32gui
from PIL import ImageDraw, ImageGrab

try:
    ctypes.windll.user32.SetProcessDPIAware()
except Exception:
    pass

BRIDGE_HOST = "127.0.0.1"
BRIDGE_PORT = 18095
CONSOLE_PORT = 18081
BRIDGE_TOKEN = "xiaojian-local-pc-bridge-2026"

CLOUD_HOST = "8.218.197.39"
CLOUD_PORT = 22
CLOUD_USER = "root"
CLOUD_PASS = "3442441682Yht"

# 常用中文别名映射，方便直接说“打开微信”“打开杀戮尖塔”“打开B站”
APP_ALIASES = {
    "微信": "Weixin",
    "wechat": "Weixin",
    "b站": "哔哩哔哩",
    "bilibili": "哔哩哔哩",
    "网易云": "网易云音乐",
    "云音乐": "网易云音乐",
    "杀戮尖塔": "Slay the Spire 2",
    "杀戮尖塔1": "Slay the Spire",
    "杀戮尖塔2": "Slay the Spire 2",
    "尖塔": "Slay the Spire 2",
    "尖塔2": "Slay the Spire 2",
    "sts2": "Slay the Spire 2",
    "星露谷": "Stardew Valley",
    "星露谷物语": "Stardew Valley",
    "泰拉瑞亚": "Terraria",
    "丝之歌": "Hollow Knight Silksong",
    "空洞骑士": "Hollow Knight Silksong",
    "弹丸论破": "Danganronpa Trigger Happy Havoc",
    "星铁": "崩坏：星穹铁道",
    "星穹铁道": "崩坏：星穹铁道",
    "崩铁": "崩坏：星穹铁道",
    "饥荒": "饥荒联机版",
    "暖雪": "暖雪 Warm Snow",
    "我的世界": "Plain Craft Launcher 2",
    "mc": "Plain Craft Launcher 2",
    "pcl": "Plain Craft Launcher 2",
    "模拟器": "MuMu模拟器",
    "mumu": "MuMu模拟器",
    "浏览器": "Google Chrome",
    "chrome": "Google Chrome",
    "edge": "Microsoft Edge",
    "vscode": "Visual Studio Code",
    "idea": "IntelliJ IDEA",
    "画图": "draw.io",
    "英雄联盟": "英雄联盟WeGame版",
    "lol": "英雄联盟WeGame版",
}

VK_MAP = {
    "space": 0x20,
    "enter": 0x0D,
    "return": 0x0D,
    "esc": 0x1B,
    "escape": 0x1B,
    "tab": 0x09,
    "backspace": 0x08,
    "delete": 0x2E,
    "del": 0x2E,
    "insert": 0x2D,
    "home": 0x24,
    "end": 0x23,
    "pageup": 0x21,
    "pgup": 0x21,
    "pagedown": 0x22,
    "pgdn": 0x22,
    "left": 0x25,
    "up": 0x26,
    "right": 0x27,
    "down": 0x28,
    "ctrl": 0x11,
    "control": 0x11,
    "alt": 0x12,
    "shift": 0x10,
    "win": 0x5B,
    "lwin": 0x5B,
    "f1": 0x70,
    "f2": 0x71,
    "f3": 0x72,
    "f4": 0x73,
    "f5": 0x74,
    "f6": 0x75,
    "f7": 0x76,
    "f8": 0x77,
    "f9": 0x78,
    "f10": 0x79,
    "f11": 0x7A,
    "f12": 0x7B,
}

active_ssh_transport = None
transport_lock = threading.Lock()


def attach_input_desktop():
    """将当前工作线程绑定到 Windows 活动交互桌面 (WinSta0\\Default)"""
    try:
        hdesk = ctypes.windll.user32.OpenInputDesktop(0, False, 0x10000000)
        if hdesk:
            ctypes.windll.user32.SetThreadDesktop(hdesk)
            ctypes.windll.user32.CloseDesktop(hdesk)
    except Exception:
        pass


def get_screen_size():
    attach_input_desktop()
    w = ctypes.windll.user32.GetSystemMetrics(0)
    h = ctypes.windll.user32.GetSystemMetrics(1)
    return w, h


def scale_coords(x, y):
    """如果模型按 1280x800 截图坐标系传入 (x<=1280, y<=800)，自动按比例映射到物理屏幕 (如 2560x1600)"""
    w, h = get_screen_size()
    ix, iy = int(x), int(y)
    if w > 1280 and 0 <= ix <= 1280 and 0 <= iy <= int(1280 * h / w) + 20:
        scale_x = w / 1280.0
        target_h = int(h * (1280.0 / w))
        scale_y = h / float(target_h)
        ix = int(ix * scale_x)
        iy = int(iy * scale_y)
    return max(0, min(ix, w - 1)), max(0, min(iy, h - 1))


def get_active_window_title():
    attach_input_desktop()
    try:
        hwnd = win32gui.GetForegroundWindow()
        return win32gui.GetWindowText(hwnd) or "桌面"
    except Exception:
        return "未知"


def list_visible_windows():
    attach_input_desktop()
    titles = []

    def enum_cb(hwnd, _):
        if win32gui.IsWindowVisible(hwnd):
            t = win32gui.GetWindowText(hwnd).strip()
            if t and t not in ("Program Manager", "Settings", "MSCTFIME UI", "Default IME"):
                titles.append(t)

    try:
        win32gui.EnumWindows(enum_cb, None)
    except Exception:
        pass
    return titles[:20]


def scan_installed_shortcuts():
    paths = [
        os.path.expanduser("~/Desktop"),
        r"C:\Users\Public\Desktop",
        os.path.expandvars(r"%APPDATA%\Microsoft\Windows\Start Menu\Programs"),
        r"C:\ProgramData\Microsoft\Windows\Start Menu\Programs",
    ]
    shortcuts = {}
    for p in paths:
        if not os.path.exists(p):
            continue
        for root, _, files in os.walk(p):
            for f in files:
                if f.lower().endswith((".lnk", ".url", ".exe", ".bat")):
                    name = os.path.splitext(f)[0]
                    if "卸载" not in name and "uninstall" not in name.lower():
                        shortcuts[name] = os.path.join(root, f)
    return shortcuts


def force_bring_to_front(hwnd):
    """突破 Windows 前台焦点锁，强制将 hwnd 窗口恢复并置顶到最前台"""
    attach_input_desktop()
    user32 = ctypes.windll.user32
    kernel32 = ctypes.windll.kernel32
    try:
        if win32gui.IsIconic(hwnd):
            win32gui.ShowWindow(hwnd, win32con.SW_RESTORE)
        else:
            win32gui.ShowWindow(hwnd, win32con.SW_SHOW)

        fg_hwnd = user32.GetForegroundWindow()
        fg_tid = user32.GetWindowThreadProcessId(fg_hwnd, None)
        cur_tid = kernel32.GetCurrentThreadId()

        if fg_tid != cur_tid and fg_tid != 0:
            user32.AttachThreadInput(cur_tid, fg_tid, True)

        # 模拟按下并释放 Alt 键以解除 Windows Foreground Lock
        user32.keybd_event(0x12, 0, 0, 0)
        user32.keybd_event(0x12, 0, 0x0002, 0)

        user32.SetWindowPos(
            hwnd,
            win32con.HWND_TOPMOST,
            0,
            0,
            0,
            0,
            win32con.SWP_NOMOVE | win32con.SWP_NOSIZE,
        )
        user32.SetWindowPos(
            hwnd,
            win32con.HWND_NOTOPMOST,
            0,
            0,
            0,
            0,
            win32con.SWP_NOMOVE | win32con.SWP_NOSIZE,
        )
        user32.SetForegroundWindow(hwnd)
        user32.BringWindowToTop(hwnd)

        if fg_tid != cur_tid and fg_tid != 0:
            user32.AttachThreadInput(cur_tid, fg_tid, False)
    except Exception:
        pass


def focus_window_by_title(keyword):
    attach_input_desktop()
    kw = keyword.lower().strip()
    if not kw:
        return False, "未提供窗口标题关键词"
    alias_kw = APP_ALIASES.get(kw, keyword).lower()
    found = []

    def enum_cb(hwnd, _):
        if win32gui.IsWindowVisible(hwnd):
            t = win32gui.GetWindowText(hwnd).strip()
            if t and (kw in t.lower() or alias_kw in t.lower()):
                found.append((hwnd, t))

    win32gui.EnumWindows(enum_cb, None)
    if not found:
        return False, f"未找到包含「{keyword}」的已打开窗口，可尝试使用 mode=\"open_app\" 启动它"
    hwnd, title = found[0]
    force_bring_to_front(hwnd)
    time.sleep(0.5)
    return True, f"已强制将窗口「{title}」切换至最前台"


def close_window_by_title(keyword):
    attach_input_desktop()
    kw = keyword.lower().strip()
    if not kw:
        return False, "未提供要关闭的窗口标题关键词"
    alias_kw = APP_ALIASES.get(kw, keyword).lower()
    found = []

    def enum_cb(hwnd, _):
        if win32gui.IsWindowVisible(hwnd):
            t = win32gui.GetWindowText(hwnd).strip()
            if t and (kw in t.lower() or alias_kw in t.lower()):
                found.append((hwnd, t))

    win32gui.EnumWindows(enum_cb, None)
    if not found:
        return False, f"未找到包含「{keyword}」的窗口"
    hwnd, title = found[0]
    try:
        win32gui.PostMessage(hwnd, win32con.WM_CLOSE, 0, 0)
        time.sleep(0.6)
        return True, f"已向窗口「{title}」发送关闭指令"
    except Exception as e:
        return False, f"关闭窗口失败: {e}"


def open_app_or_url(app_name):
    attach_input_desktop()
    raw = app_name.strip()
    if not raw:
        return False, "未提供应用名称或路径"

    # 1. URL 或 绝对路径
    if raw.startswith(("http://", "https://")) or os.path.exists(raw):
        try:
            os.startfile(raw)
            time.sleep(2.0)
            return True, f"已打开：{raw}"
        except Exception as e:
            return False, f"打开失败: {e}"

    # 1.5 针对《杀戮尖塔2》优先确保处于2号练习存档槽位并支持直接从Steam安装目录启动
    if raw.lower() in ("杀戮尖塔", "杀戮尖塔2", "尖塔", "尖塔2", "sts2", "slay the spire 2", "slaythespire2"):
        ensure_sts2_profile2()
        ok_focus, _ = focus_window_by_title("Slay the Spire 2")
        if ok_focus:
            return True, "已将运行中的《杀戮尖塔2》（2号练习存档槽位）切换至最前台\n" + get_sts2_live_report()
        sts2_exe = r"D:\SteamLibrary\steamapps\common\Slay the Spire 2\SlayTheSpire2.exe"
        if os.path.exists(sts2_exe):
            try:
                subprocess.Popen([sts2_exe], cwd=os.path.dirname(sts2_exe))
                time.sleep(5.0)
                focus_window_by_title("Slay the Spire 2")
                return True, f"已启动《杀戮尖塔2》（已自动锁定为2号练习存档槽位，路径：{sts2_exe}）\n" + get_sts2_live_report()
            except Exception as e:
                return False, f"启动《杀戮尖塔2》失败: {e}"

    # 2. 如果窗口已经打开，优先把它切到最前台，再同时触发启动
    target = APP_ALIASES.get(raw.lower(), raw)
    shortcuts = scan_installed_shortcuts()

    # 精确匹配 -> 包含匹配
    matched_path = None
    matched_name = None
    for name, path in shortcuts.items():
        if name.lower() == target.lower() or name.lower() == raw.lower():
            matched_path = path
            matched_name = name
            break
    if not matched_path:
        for name, path in shortcuts.items():
            if target.lower() in name.lower() or raw.lower() in name.lower():
                matched_path = path
                matched_name = name
                break

    if matched_path:
        try:
            os.startfile(matched_path)
            time.sleep(2.2)
            # 尝试将新启动的窗口置顶
            focus_window_by_title(matched_name)
            return True, f"已成功启动应用/游戏：「{matched_name}」（路径：{matched_path}）"
        except Exception as e:
            return False, f"启动快捷方式「{matched_name}」失败: {e}"

    # 3. 尝试通过系统命令 / Start-Process 启动（如 notepad, calc, explorer, cmd 等）
    try:
        subprocess.Popen(["cmd", "/c", "start", "", raw], shell=False)
        time.sleep(1.8)
        focus_window_by_title(raw)
        return True, f"已通过系统命令启动：{raw}"
    except Exception as e:
        return False, f"未找到名为「{raw}」的应用且系统启动失败: {e}"


def click_screen(x, y, button="left"):
    attach_input_desktop()
    tx, ty = scale_coords(x, y)
    ctypes.windll.user32.SetCursorPos(tx, ty)
    time.sleep(0.08)
    if button == "right":
        ctypes.windll.user32.mouse_event(0x0008, 0, 0, 0, 0)
        ctypes.windll.user32.mouse_event(0x0010, 0, 0, 0, 0)
    elif button == "double":
        for _ in range(2):
            ctypes.windll.user32.mouse_event(0x0002, 0, 0, 0, 0)
            ctypes.windll.user32.mouse_event(0x0004, 0, 0, 0, 0)
            time.sleep(0.06)
    else:
        ctypes.windll.user32.mouse_event(0x0002, 0, 0, 0, 0)
        ctypes.windll.user32.mouse_event(0x0004, 0, 0, 0, 0)
    time.sleep(0.5)
    return f"已在物理坐标 ({tx}, {ty}) 执行鼠标 {button} 点击"


def drag_screen(x, y, to_x, to_y):
    attach_input_desktop()
    sx, sy = scale_coords(x, y)
    ex, ey = scale_coords(to_x, to_y)
    ctypes.windll.user32.SetCursorPos(sx, sy)
    time.sleep(0.1)
    ctypes.windll.user32.mouse_event(0x0002, 0, 0, 0, 0)
    time.sleep(0.1)
    steps = 12
    for i in range(1, steps + 1):
        cx = int(sx + (ex - sx) * i / steps)
        cy = int(sy + (ey - sy) * i / steps)
        ctypes.windll.user32.SetCursorPos(cx, cy)
        time.sleep(0.02)
    time.sleep(0.1)
    ctypes.windll.user32.mouse_event(0x0004, 0, 0, 0, 0)
    time.sleep(0.5)
    return f"已从 ({sx}, {sy}) 拖拽至 ({ex}, {ey})"


def scroll_screen(x, y, amount=-3):
    attach_input_desktop()
    if x is not None and y is not None:
        tx, ty = scale_coords(x, y)
        ctypes.windll.user32.SetCursorPos(tx, ty)
        time.sleep(0.05)
    delta = int(amount) * 120
    ctypes.windll.user32.mouse_event(0x0800, 0, 0, delta, 0)
    time.sleep(0.4)
    return f"已滚动鼠标滚轮 (步长 {amount})"


def resolve_vk(k):
    k = k.lower().strip()
    if k in VK_MAP:
        return VK_MAP[k]
    if len(k) == 1 and k.isalnum():
        return ord(k.upper())
    return None


def press_key_or_hotkey(key_expr):
    """支持单键 (如 space, enter) 或组合键 (如 alt+tab, ctrl+v, win+d, ctrl+shift+esc)"""
    attach_input_desktop()
    parts = [p.strip() for p in key_expr.lower().replace("-", "+").split("+") if p.strip()]
    if not parts:
        return False, "未指定按键"
    vks = []
    for p in parts:
        vk = resolve_vk(p)
        if vk is None:
            return False, f"不支持的按键名: {p}"
        vks.append(vk)

    for vk in vks:
        ctypes.windll.user32.keybd_event(vk, 0, 0, 0)
        time.sleep(0.04)
    time.sleep(0.06)
    for vk in reversed(vks):
        ctypes.windll.user32.keybd_event(vk, 0, 0x0002, 0)
        time.sleep(0.04)
    time.sleep(0.5)
    return True, f"已按下按键/组合键: {key_expr}"


def type_text_via_clipboard(text, press_enter=False):
    """通过剪贴板粘贴输入任意中英文文本，避免输入法干扰"""
    attach_input_desktop()
    try:
        win32clipboard.OpenClipboard()
        win32clipboard.EmptyClipboard()
        win32clipboard.SetClipboardText(text, win32con.CF_UNICODETEXT)
        win32clipboard.CloseClipboard()
    except Exception as e:
        return False, f"写入剪贴板失败: {e}"

    time.sleep(0.08)
    press_key_or_hotkey("ctrl+v")
    if press_enter:
        time.sleep(0.1)
        press_key_or_hotkey("enter")
    return True, f"已输入文本：「{text}」" + (" 并按下回车" if press_enter else "")


def run_local_powershell(command):
    attach_input_desktop()
    if not command or not command.strip():
        return False, "未提供要执行的命令"
    try:
        proc = subprocess.run(
            ["powershell", "-NoProfile", "-Command", command],
            capture_output=True,
            text=True,
            timeout=15,
        )
        out = (proc.stdout or "") + (proc.stderr or "")
        out = out.strip()[:1200]
        return True, f"命令退出码 {proc.returncode}，输出：\n{out if out else '(无输出)'}"
    except Exception as e:
        return False, f"执行命令失败: {e}"


def capture_screenshot_b64():
    attach_input_desktop()
    img = ImageGrab.grab()
    pt = ctypes.wintypes.POINT()
    ctypes.windll.user32.GetCursorPos(ctypes.byref(pt))
    draw = ImageDraw.Draw(img)
    r = 10
    draw.ellipse((pt.x - r, pt.y - r, pt.x + r, pt.y + r), outline="red", width=3)
    draw.line((pt.x - 16, pt.y, pt.x + 16, pt.y), fill="red", width=2)
    draw.line((pt.x, pt.y - 16, pt.x, pt.y + 16), fill="red", width=2)

    if img.width > 1280:
        new_h = int(img.height * (1280.0 / img.width))
        img = img.resize((1280, new_h))
    buf = io.BytesIO()
    img.convert("RGB").save(buf, format="JPEG", quality=82)
    return base64.b64encode(buf.getvalue()).decode("ascii")


# ==================== 《杀戮尖塔2》专属状态解析、攻略联动与复盘系统 ====================

STS2_APPDATA = r"C:\Users\Moon\AppData\Roaming\SlayTheSpire2"
STS2_STEAM_DIR = os.path.join(STS2_APPDATA, "steam", "76561199387227940")
STS2_LOG_PATH = os.path.join(STS2_APPDATA, "logs", "godot.log")
STS2_KNOWLEDGE_DIR = os.path.abspath(
    os.path.join(os.path.dirname(__file__), "..", "data", "sts2_knowledge")
)
_STS2_DB_CACHE = None


def load_sts2_zhs_db():
    global _STS2_DB_CACHE
    if _STS2_DB_CACHE is not None:
        return _STS2_DB_CACHE
    db_path = os.path.join(STS2_KNOWLEDGE_DIR, "sts2_zhs_db.json")
    if os.path.exists(db_path):
        try:
            with open(db_path, "r", encoding="utf-8") as f:
                _STS2_DB_CACHE = json.load(f)
                return _STS2_DB_CACHE
        except Exception:
            pass
    _STS2_DB_CACHE = {"cards": {}, "relics": {}, "monsters": {}, "powers": {}, "potions": {}, "characters": {}}
    return _STS2_DB_CACHE


def ensure_sts2_profile2():
    """强制确保《杀戮尖塔2》处于2号存档槽位(profile2)，保护主人的1号存档槽位(profile1)"""
    for sub in ("", "modded"):
        base_sub = os.path.join(STS2_STEAM_DIR, sub) if sub else STS2_STEAM_DIR
        if not os.path.exists(base_sub):
            continue
        prof_save = os.path.join(base_sub, "profile.save")
        try:
            with open(prof_save, "w", encoding="utf-8") as f:
                json.dump({"last_profile_id": 2, "schema_version": 2}, f, indent=2)
        except Exception:
            pass


def translate_sts2_log_line(line):
    db = load_sts2_zhs_db()
    out = line.strip()
    for prefix, cat in (
        ("playing card ", "cards"),
        ("RELIC.", "relics"),
        ("CHARACTER.", "characters"),
        ("MONSTER.", "monsters"),
        ("ENCOUNTER.", "monsters"),
    ):
        if prefix in out:
            for token in out.replace("(", " ").replace(")", " ").split():
                clean_tok = token.replace("RELIC.", "").replace("CHARACTER.", "").replace("MONSTER.", "").replace("ENCOUNTER.", "")
                info = db.get(cat, {}).get(clean_tok)
                if isinstance(info, dict) and info.get("title"):
                    out = out.replace(token, f"{token}({info['title']})")
                elif isinstance(info, str) and info:
                    out = out.replace(token, f"{token}({info})")
    return out


def get_sts2_recent_logs(max_lines=16):
    if not os.path.exists(STS2_LOG_PATH):
        return []
    try:
        with open(STS2_LOG_PATH, "r", encoding="utf-8", errors="ignore") as f:
            lines = f.readlines()[-120:]
        interesting = []
        for ln in lines:
            if any(
                k in ln
                for k in (
                    "Player 1 playing card",
                    "Player 1 obtained",
                    "Player 1 selected",
                    "Monster ",
                    "fought ",
                    "Card rarity",
                    "current_run.save",
                    "died",
                    "Defeated",
                )
            ):
                interesting.append(translate_sts2_log_line(ln))
        return interesting[-max_lines:]
    except Exception:
        return []


def get_sts2_live_report():
    """读取本机《杀戮尖塔2》2号槽位的实时存档、官方中文卡牌/遗物效果、最新战斗日志、局内流派便签与避坑铁律"""
    db = load_sts2_zhs_db()
    lines = ["【《杀戮尖塔2》后台实时数据与攻略复盘面板（当前锁定：2号练习存档槽位 profile2）】"]

    # 1. 查找2号槽位的 current_run.save
    candidates = [
        os.path.join(STS2_STEAM_DIR, "modded", "profile2", "saves", "current_run.save"),
        os.path.join(STS2_STEAM_DIR, "profile2", "saves", "current_run.save"),
    ]
    existing = [p for p in candidates if os.path.exists(p)]
    run_file = max(existing, key=lambda p: os.path.getmtime(p)) if existing else None

    active_char_cn = ""
    if run_file:
        try:
            with open(run_file, "r", encoding="utf-8") as f:
                rd = json.load(f)
            asc = rd.get("ascension", 0)
            act_idx = rd.get("current_act_index", 0) + 1
            coords = rd.get("visited_map_coords", [])
            floor_num = len(coords)
            p0 = (rd.get("players") or [{}])[0]
            char_id = p0.get("character_id", p0.get("character", "")).replace("CHARACTER.", "")
            active_char_cn = db.get("characters", {}).get(char_id, {}).get("title", char_id)
            cur_hp = p0.get("current_hp", "?")
            max_hp = p0.get("max_hp", "?")
            gold = p0.get("gold", "?")

            # 统计卡组与官方中文效果
            deck_counts = {}
            for c in p0.get("deck", []):
                cid = c.get("id", "").replace("CARD.", "")
                up = "+" if c.get("upgraded") or c.get("upgrade_level", 0) > 0 else ""
                key = f"{cid}{up}"
                deck_counts[key] = deck_counts.get(key, 0) + 1
            deck_strs = []
            for key, cnt in deck_counts.items():
                raw_id = key.rstrip("+")
                up_mark = "+" if key.endswith("+") else ""
                cinfo = db.get("cards", {}).get(raw_id, {})
                cname = cinfo.get("title", raw_id) + up_mark
                cdesc = cinfo.get("desc", "")
                if cdesc:
                    deck_strs.append(f"{cname}x{cnt}({cdesc[:38]})")
                else:
                    deck_strs.append(f"{cname}x{cnt}")

            # 统计遗物、海克斯符文(HextechRunes)与药水
            relic_strs = []
            hextech_strs = []
            for r in p0.get("relics", []):
                rid = (r.get("id", "") if isinstance(r, dict) else str(r)).replace("RELIC.", "")
                rinfo = db.get("relics", {}).get(rid, {})
                rname = rinfo.get("title", rid)
                rdesc = rinfo.get("desc", "")
                rmod = rinfo.get("mod", "")
                item_text = f"{rname}({rdesc[:36]})" if rdesc else rname
                if rmod in ("HextechRunes", "HextechRunesSponsorPack") or any(
                    w in rid.upper() for w in ("RUNE", "HEX", "FORGE")
                ):
                    hextech_strs.append(item_text)
                else:
                    relic_strs.append(item_text)

            potion_strs = []
            for pot in p0.get("potions", []):
                pid = (pot.get("id", "") if isinstance(pot, dict) else str(pot)).replace("POTION.", "")
                if pid and "EMPTY" not in pid:
                    pinfo = db.get("potions", {}).get(pid, {})
                    pname = pinfo.get("title", pid)
                    pdesc = pinfo.get("desc", "")
                    potion_strs.append(f"{pname}({pdesc[:28]})" if pdesc else pname)

            lines.append(
                f"- 当前实时存档：角色={active_char_cn}({char_id}) | 进阶={asc} | 第{act_idx}幕-已走{floor_num}层 | 生命={cur_hp}/{max_hp} | 金币={gold}"
            )
            if hextech_strs:
                lines.append(f"- 当前激活的海克斯符文/锻造器({len(hextech_strs)})：{'；'.join(hextech_strs)}")
            lines.append(f"- 当前遗物({len(relic_strs)})：{'；'.join(relic_strs) if relic_strs else '无'}")
            if potion_strs:
                lines.append(f"- 当前药水：{'；'.join(potion_strs)}")
            lines.append(
                f"- 当前牌组({sum(deck_counts.values())}张)：{' | '.join(deck_strs[:24])}"
            )
        except Exception as e:
            lines.append(f"- 解析 current_run.save 异常: {e}")
    else:
        lines.append("- 当前实时存档：2号存档槽位暂无进行中的战局（处于主菜单、角色选择或刚结束上一局结算界面）。")

    # 2. 读取局内流派规划便签
    plan_file = os.path.join(STS2_KNOWLEDGE_DIR, "sts2_current_plan.json")
    if os.path.exists(plan_file):
        try:
            with open(plan_file, "r", encoding="utf-8") as f:
                plan = json.load(f)
            lines.append(
                f"- 局内流派便签：目标流派=「{plan.get('target_archetype','未定')}」 | 急需牌={plan.get('cards_needed',[])} | 禁拿牌={plan.get('cards_avoid',[])} | 备注={plan.get('current_floor_note','')}"
            )
        except Exception:
            pass

    # 3. 读取最新战斗日志與避坑铁律
    rec_logs = get_sts2_recent_logs(12)
    if rec_logs:
        lines.append("- 最近后台战斗与出牌日志(godot.log)：\n  " + "\n  ".join(rec_logs))

    lessons_file = os.path.join(STS2_KNOWLEDGE_DIR, "sts2_lessons_learned.json")
    if os.path.exists(lessons_file):
        try:
            with open(lessons_file, "r", encoding="utf-8") as f:
                les = json.load(f)
            rules = les.get("global_iron_rules", [])[:3]
            if rules:
                lines.append("- 全局爬塔铁律：" + "；".join(rules))
            # 检查最近日志里有没有命中特定怪物的避坑教训
            joined_log = "\n".join(rec_logs)
            for ename, edata in les.get("enemy_specific_lessons", {}).items():
                if ename in joined_log:
                    lines.append(f"- 【高危怪物避坑铁律-{ename}】：{edata.get('iron_rule','')}")
        except Exception:
            pass

    lines.append(
        "- 工坊模组提示：已启用【海克斯大乱斗+拓展包】（每幕开局必选1个海克斯符文，每个槽位可重随2次，点击卡片即直接生效；火堆支持『添柴』删牌；奖励支持『锻体』选属性锻造器）、【快速SL按钮】（战斗陷入绝境时可按 Esc 打开暂停菜单点击『快速SL』重打当前房间1次）、【皮肤切换器】（自定义立绘下请以中文文字和后台ID为准，勿仅凭长相猜怪）。"
    )
    lines.append(
        "- 快捷键提示：手牌对应数字键 1-9（无目标牌按数字键后点屏幕中央 640,400；指向攻击牌按数字键后点目标怪物身体）；按 E 键结束回合；按 M 键看地图。建议优先用 mode=\"sts2_batch\" 一次性下达整回合动作！"
    )
    return "\n".join(lines)


def execute_sts2_batch(steps, wait_after_ms=2200):
    """单回合批量执行多步按键/点击/拖拽动作，省去一步一截图的额度消耗"""
    attach_input_desktop()
    focus_window_by_title("Slay the Spire 2")
    if not isinstance(steps, list) or not steps:
        return False, "sts2_batch 需提供非空的 steps 动作列表"

    executed = []
    for idx, st in enumerate(steps[:15], 1):
        if not isinstance(st, dict):
            continue
        stype = str(st.get("type") or st.get("action") or "key").lower()
        if stype in ("key", "hotkey"):
            k = str(st.get("key", "space"))
            ok, msg = press_key_or_hotkey(k)
            executed.append(f"步{idx}[按键 {k}]: {'成功' if ok else msg}")
        elif stype == "click":
            x = st.get("x", 640)
            y = st.get("y", 400)
            btn = st.get("button", "left")
            msg = click_screen(x, y, btn)
            executed.append(f"步{idx}[点击 ({x},{y})]: {msg}")
        elif stype == "drag":
            msg = drag_screen(
                st.get("x", 640),
                st.get("y", 700),
                st.get("to_x", 640),
                st.get("to_y", 380),
            )
            executed.append(f"步{idx}[拖拽]: {msg}")
        elif stype == "wait":
            ms = max(100, min(int(st.get("ms", 800)), 5000))
            time.sleep(ms / 1000.0)
            executed.append(f"步{idx}[等待 {ms}ms]")
        time.sleep(0.32)

    wait_s = max(0.3, min(float(wait_after_ms or 2200) / 1000.0, 6.0))
    time.sleep(wait_s)
    summary = "已连贯执行《杀戮尖塔2》批量操作：\n" + "\n".join(executed) + "\n\n" + get_sts2_live_report()
    return True, summary


def update_sts2_review(req):
    """更新局内流派定型便签，或写入阵亡/通关后的死因复盘与避坑铁律"""
    os.makedirs(STS2_KNOWLEDGE_DIR, exist_ok=True)
    plan_file = os.path.join(STS2_KNOWLEDGE_DIR, "sts2_current_plan.json")
    lessons_file = os.path.join(STS2_KNOWLEDGE_DIR, "sts2_lessons_learned.json")
    notes_done = []

    # 1. 更新局内流派便签
    if any(k in req for k in ("target_archetype", "plan_note", "cards_needed", "cards_avoid", "character")):
        try:
            plan = {}
            if os.path.exists(plan_file):
                with open(plan_file, "r", encoding="utf-8") as f:
                    plan = json.load(f)
            plan["profile_slot"] = 2
            if req.get("character"):
                plan["character"] = str(req["character"])
            if req.get("target_archetype"):
                plan["target_archetype"] = str(req["target_archetype"])
            if req.get("cards_needed"):
                plan["cards_needed"] = req["cards_needed"]
            if req.get("cards_avoid"):
                plan["cards_avoid"] = req["cards_avoid"]
            if req.get("plan_note"):
                plan["current_floor_note"] = str(req["plan_note"])
            with open(plan_file, "w", encoding="utf-8") as f:
                json.dump(plan, f, ensure_ascii=False, indent=2)
            notes_done.append(f"已更新局内流派规划便签（目标流派：{plan.get('target_archetype')}）")
        except Exception as e:
            return False, f"更新流派便签失败: {e}"

    # 2. 写入死因复盘与怪物避坑铁律
    if any(k in req for k in ("enemy_name", "death_cause", "iron_rule", "run_summary")):
        try:
            les = {"version": 1, "global_iron_rules": [], "enemy_specific_lessons": {}, "run_reviews": []}
            if os.path.exists(lessons_file):
                with open(lessons_file, "r", encoding="utf-8") as f:
                    les = json.load(f)
            now_str = time.strftime("%Y-%m-%d %H:%M:%S")
            les["updated_at"] = now_str
            ename = str(req.get("enemy_name") or "").strip()
            if ename and (req.get("death_cause") or req.get("iron_rule")):
                prev = les.setdefault("enemy_specific_lessons", {}).get(ename, {})
                les["enemy_specific_lessons"][ename] = {
                    "deaths_recorded": int(prev.get("deaths_recorded", 0)) + 1,
                    "death_cause": str(req.get("death_cause") or prev.get("death_cause", "")),
                    "iron_rule": str(req.get("iron_rule") or prev.get("iron_rule", "")),
                    "updated_at": now_str,
                }
                notes_done.append(f"已将「{ename}」的死因与避坑铁律写入长期经验库")
            if req.get("run_summary") or req.get("iron_rule"):
                les.setdefault("run_reviews", []).append(
                    {
                        "date": now_str,
                        "character": str(req.get("character") or "未知"),
                        "enemy_name": ename,
                        "death_cause": str(req.get("death_cause") or ""),
                        "iron_rule": str(req.get("iron_rule") or ""),
                        "summary": str(req.get("run_summary") or req.get("iron_rule") or ""),
                    }
                )
                notes_done.append("已归档本局实战复盘记录")
            with open(lessons_file, "w", encoding="utf-8") as f:
                json.dump(les, f, ensure_ascii=False, indent=2)
        except Exception as e:
            return False, f"写入死因复盘库失败: {e}"

    if not notes_done:
        return True, "未传入更新字段，当前状态如下：\n" + get_sts2_live_report()
    return True, "；".join(notes_done) + "\n\n" + get_sts2_live_report()


class BridgeHandler(BaseHTTPRequestHandler):
    def log_message(self, fmt, *args):
        pass

    def do_POST(self):
        attach_input_desktop()
        token = self.headers.get("X-Bridge-Token", "")
        if token != BRIDGE_TOKEN:
            self.send_response(403)
            self.end_headers()
            return

        length = int(self.headers.get("Content-Length", "0"))
        raw = self.rfile.read(length) if length > 0 else b"{}"
        try:
            req = json.loads(raw.decode("utf-8"))
        except Exception:
            req = {}

        mode = req.get("mode", "status")
        action_note = "已读取本地电脑状态"
        ok = True
        err_msg = None

        if mode == "open_app":
            ok, msg = open_app_or_url(str(req.get("app_name") or req.get("window_title") or ""))
            if ok:
                action_note = msg
            else:
                err_msg = msg
        elif mode == "focus_window":
            ok, msg = focus_window_by_title(str(req.get("window_title") or req.get("app_name") or ""))
            if ok:
                action_note = msg
            else:
                err_msg = msg
        elif mode == "close_window":
            ok, msg = close_window_by_title(str(req.get("window_title") or req.get("app_name") or ""))
            if ok:
                action_note = msg
            else:
                err_msg = msg
        elif mode == "click":
            x = req.get("x", 640)
            y = req.get("y", 400)
            btn = req.get("button", "left")
            action_note = click_screen(x, y, btn)
        elif mode == "drag":
            action_note = drag_screen(
                req.get("x", 640),
                req.get("y", 400),
                req.get("to_x", 640),
                req.get("to_y", 200),
            )
        elif mode == "scroll":
            action_note = scroll_screen(
                req.get("x"),
                req.get("y"),
                req.get("scroll_amount", -3),
            )
        elif mode in ("key", "hotkey"):
            ok, msg = press_key_or_hotkey(str(req.get("key", "space")))
            if ok:
                action_note = msg
            else:
                err_msg = msg
        elif mode == "type_text":
            ok, msg = type_text_via_clipboard(
                str(req.get("text", "")),
                bool(req.get("press_enter", False)),
            )
            if ok:
                action_note = msg
            else:
                err_msg = msg
        elif mode == "run_command":
            ok, msg = run_local_powershell(str(req.get("command", "")))
            if ok:
                action_note = msg
            else:
                err_msg = msg
        elif mode == "screenshot":
            action_note = "已截取本地电脑实时画面"
        elif mode == "sts2_status":
            ensure_sts2_profile2()
            focus_window_by_title("Slay the Spire 2")
            action_note = get_sts2_live_report()
        elif mode == "sts2_batch":
            ensure_sts2_profile2()
            ok, msg = execute_sts2_batch(
                req.get("steps", []),
                req.get("wait_after_ms", 2200),
            )
            if ok:
                action_note = msg
            else:
                err_msg = msg
        elif mode == "sts2_review":
            ok, msg = update_sts2_review(req)
            if ok:
                action_note = msg
            else:
                err_msg = msg

        active_title = get_active_window_title()
        if (
            ok
            and mode not in ("status", "sts2_status", "sts2_batch", "sts2_review")
            and "slay the spire 2" in active_title.lower()
            and "后台实时数据与攻略复盘面板" not in action_note
        ):
            action_note += "\n\n" + get_sts2_live_report()

        w, h = get_screen_size()
        img_b64 = ""
        if ok and mode != "status":
            try:
                img_b64 = capture_screenshot_b64()
            except Exception as e:
                action_note += f"（截屏异常: {e}）"

        installed_apps = []
        if mode == "status":
            installed_apps = sorted(list(scan_installed_shortcuts().keys()))[:60]

        resp = {
            "ok": ok,
            "error": err_msg,
            "action_note": action_note,
            "active_window": get_active_window_title(),
            "screen_size": f"{w}x{h} (视觉点击坐标系支持 1280x800 或原生 {w}x{h})",
            "windows": list_visible_windows(),
            "installed_apps": installed_apps,
            "image_base64": img_b64,
        }
        payload = json.dumps(resp).encode("utf-8")
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(payload)))
        self.end_headers()
        self.wfile.write(payload)


def pipe_sockets(chan, sock):
    def forward(src, dst):
        try:
            while True:
                data = src.recv(16384)
                if not data:
                    break
                dst.sendall(data)
        except Exception:
            pass
        finally:
            try:
                src.close()
            except Exception:
                pass
            try:
                dst.close()
            except Exception:
                pass

    t1 = threading.Thread(target=forward, args=(chan, sock), daemon=True)
    t2 = threading.Thread(target=forward, args=(sock, chan), daemon=True)
    t1.start()
    t2.start()


def handle_reverse_channel(chan):
    try:
        sock = socket.create_connection((BRIDGE_HOST, BRIDGE_PORT), timeout=5)
        pipe_sockets(chan, sock)
    except Exception:
        try:
            chan.close()
        except Exception:
            pass


def start_console_forward_listener():
    """本地 127.0.0.1:18081 -> 云端 127.0.0.1:18081 正向隧道（管理台开机直达）"""
    try:
        srv = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
        srv.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
        srv.bind(("127.0.0.1", CONSOLE_PORT))
        srv.listen(32)
    except Exception:
        return

    def accept_loop():
        while True:
            try:
                client_sock, addr = srv.accept()
                with transport_lock:
                    tr = active_ssh_transport
                if tr is None or not tr.is_active():
                    client_sock.close()
                    continue
                chan = tr.open_channel("direct-tcpip", ("127.0.0.1", CONSOLE_PORT), addr)
                pipe_sockets(chan, client_sock)
            except Exception:
                time.sleep(0.2)

    threading.Thread(target=accept_loop, daemon=True).start()


def reverse_tunnel_loop():
    global active_ssh_transport
    while True:
        try:
            client = paramiko.SSHClient()
            client.set_missing_host_key_policy(paramiko.AutoAddPolicy())
            client.connect(CLOUD_HOST, CLOUD_PORT, CLOUD_USER, CLOUD_PASS, timeout=15)
            _, out, _ = client.exec_command(f"fuser -k {BRIDGE_PORT}/tcp 2>/dev/null || true")
            out.channel.recv_exit_status()
            time.sleep(0.3)

            transport = client.get_transport()
            transport.set_keepalive(20)
            with transport_lock:
                active_ssh_transport = transport
            transport.request_port_forward("127.0.0.1", BRIDGE_PORT)
            print(f"[BRIDGE] 已成功挂载至云端小简 (云端 127.0.0.1:{BRIDGE_PORT} -> 本机 {BRIDGE_HOST}:{BRIDGE_PORT})")
            while transport.is_active():
                chan = transport.accept(timeout=5)
                if chan is not None:
                    threading.Thread(target=handle_reverse_channel, args=(chan,), daemon=True).start()
        except Exception as e:
            print(f"[BRIDGE] 反向隧道断开或重连中: {e}")
        time.sleep(5)


def main():
    try:
        server = HTTPServer((BRIDGE_HOST, BRIDGE_PORT), BridgeHandler)
    except OSError:
        return
    threading.Thread(target=server.serve_forever, daemon=True).start()
    start_console_forward_listener()
    print(f"[BRIDGE] 本地电脑全功能控制服务已启动于 http://{BRIDGE_HOST}:{BRIDGE_PORT}")
    reverse_tunnel_loop()


if __name__ == "__main__":
    main()
