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
from http.server import BaseHTTPRequestHandler, HTTPServer, ThreadingHTTPServer

import paramiko
import win32clipboard
import win32con
import win32gui
import win32process
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
    priority = [t for t in titles if any(w in t.lower() for w in ("slay", "spire", "杀戮", "尖塔"))]
    others = [t for t in titles if t not in priority]
    return (priority + others)[:35]


def scan_installed_shortcuts():
    paths = [
        os.path.expanduser("~/Desktop"),
        r"C:\Users\Public\Desktop",
        os.path.expandvars(r"%APPDATA%\Microsoft\Windows\Start Menu\Programs"),
        r"C:\ProgramData\Microsoft\Windows\Start Menu\Programs",
    ]
    ignore_keywords = (
        "卸载",
        "uninstall",
        "administrative tools",
        "application verifier",
        "component services",
        "computer management",
        "control panel",
        "debuggable package",
        "developer command",
        "developer powershell",
        "disk cleanup",
        "documentation for",
        "error reporter",
        "ea updater",
        "更新程序",
        "event viewer",
        "git faqs",
        "git release notes",
        "install additional tools",
        "character map",
    )
    shortcuts = {}
    # 优先注入本机 Steam 游戏库已安装的核心游戏
    steam_games = {
        "杀戮尖塔2 (Slay the Spire 2)": r"D:\SteamLibrary\steamapps\common\Slay the Spire 2\SlayTheSpire2.exe",
        "Slay the Spire 2": r"D:\SteamLibrary\steamapps\common\Slay the Spire 2\SlayTheSpire2.exe",
        "杀戮尖塔1 (Slay the Spire)": r"D:\SteamLibrary\steamapps\common\SlayTheSpire\SlayTheSpire.exe",
    }
    for gname, gpath in steam_games.items():
        if os.path.exists(gpath):
            shortcuts[gname] = gpath

    for p in paths:
        if not os.path.exists(p):
            continue
        for root, _, files in os.walk(p):
            for f in files:
                if f.lower().endswith((".lnk", ".url", ".exe", ".bat")):
                    name = os.path.splitext(f)[0]
                    low = name.lower()
                    if not any(k in low for k in ignore_keywords):
                        shortcuts[name] = os.path.join(root, f)
    return shortcuts


def force_bring_to_front(hwnd):
    """突破 Windows 前台焦点锁，强制将 hwnd 窗口恢复并置顶到最前台"""
    attach_input_desktop()
    user32 = ctypes.windll.user32
    kernel32 = ctypes.windll.kernel32
    try:
        fg_hwnd = user32.GetForegroundWindow()
        if fg_hwnd == hwnd and not win32gui.IsIconic(hwnd):
            return False

        if win32gui.IsIconic(hwnd):
            win32gui.ShowWindow(hwnd, win32con.SW_RESTORE)
        else:
            win32gui.ShowWindow(hwnd, win32con.SW_SHOW)

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
        return True
    except Exception:
        return False


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
    if force_bring_to_front(hwnd):
        time.sleep(0.15)
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
        if launch_sts2_on_interactive_desktop():
            return True, "已启动《杀戮尖塔2》（已自动锁定为2号练习存档槽位并切换至最前台）\n" + get_sts2_live_report()
        return False, "启动《杀戮尖塔2》失败"

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
            spawn_on_default_desktop(f'cmd.exe /c start "" "{matched_path}"')
            time.sleep(2.2)
            # 尝试将新启动的窗口置顶
            focus_window_by_title(matched_name)
            return True, f"已成功启动应用/游戏：「{matched_name}」（路径：{matched_path}）"
        except Exception as e:
            return False, f"启动快捷方式「{matched_name}」失败: {e}"

    # 3. 尝试通过系统命令 / Start-Process 启动（如 notepad, calc, explorer, cmd 等）
    try:
        spawn_on_default_desktop(f'cmd.exe /c start "" "{raw}"')
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
    time.sleep(0.15)
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
        time.sleep(0.03)
    time.sleep(0.15)
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


def capture_screenshot_b64(draw_grid=True):
    attach_input_desktop()
    raw_img = ImageGrab.grab()
    orig_w, orig_h = raw_img.width, raw_img.height
    pt = ctypes.wintypes.POINT()
    ctypes.windll.user32.GetCursorPos(ctypes.byref(pt))

    if orig_w > 1280:
        new_h = int(orig_h * (1280.0 / orig_w))
        img = raw_img.resize((1280, new_h))
        mx = int(pt.x * (1280.0 / orig_w))
        my = int(pt.y * (new_h / float(orig_h)))
    else:
        img = raw_img.copy()
        mx, my = pt.x, pt.y

    draw = ImageDraw.Draw(img)
    # 绘制 1280x800 视觉坐标辅助刻度网格（使用整条细线避免 JPEG 高频噪点膨胀）
    if draw_grid:
        w, h = img.width, img.height
        for gx in range(100, w, 100):
            draw.line((gx, 0, gx, h), fill=(0, 140, 170), width=1)
            draw.rectangle((gx - 14, 2, gx + 16, 15), fill=(0, 0, 0))
            draw.text((gx - 12, 3), str(gx), fill=(255, 240, 0))
        for gy in range(100, h, 100):
            draw.line((0, gy, w, gy), fill=(0, 140, 170), width=1)
            draw.rectangle((2, gy - 7, 28, gy + 7), fill=(0, 0, 0))
            draw.text((4, gy - 6), str(gy), fill=(255, 240, 0))
        for gx in (200, 400, 640, 880, 1100):
            for gy in (150, 300, 450, 600, 720):
                if gx < w and gy < h:
                    lbl = f"{gx},{gy}"
                    draw.rectangle((gx - 2, gy - 2, gx + 44, gy + 11), fill=(0, 0, 0))
                    draw.text((gx, gy - 1), lbl, fill=(0, 255, 180))

    # 绘制当前鼠标红十字
    r = 8
    draw.ellipse((mx - r, my - r, mx + r, my + r), outline="red", width=2)
    draw.line((mx - 14, my, mx + 14, my), fill="red", width=2)
    draw.line((mx, my - 14, mx, my + 14), fill="red", width=2)

    buf = io.BytesIO()
    img.convert("RGB").save(buf, format="JPEG", quality=72)
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


def spawn_on_default_desktop(cmd_line, cwd=None):
    """显式指定 lpDesktop = 'winsta0\\default' 在用户真实物理交互桌面上创建 GUI 进程"""
    si = win32process.STARTUPINFO()
    si.lpDesktop = "winsta0\\default"
    si.dwFlags = win32con.STARTF_USESHOWWINDOW
    si.wShowWindow = win32con.SW_SHOWNORMAL
    win32process.CreateProcess(
        None,
        cmd_line,
        None,
        None,
        False,
        0,
        None,
        cwd,
        si,
    )


def launch_sts2_on_interactive_desktop():
    """通过 CreateProcess(lpDesktop='winsta0\\default') 启动《杀戮尖塔2》，确保100%在用户可见桌面上创建窗口"""
    attach_input_desktop()
    ensure_sts2_profile2()
    ok_focus, _ = focus_window_by_title("Slay the Spire 2")
    if ok_focus:
        return True
    sts2_exe = r"D:\SteamLibrary\steamapps\common\Slay the Spire 2\SlayTheSpire2.exe"
    sts2_dir = os.path.dirname(sts2_exe)
    appid_file = os.path.join(sts2_dir, "steam_appid.txt")
    if not os.path.exists(appid_file):
        try:
            with open(appid_file, "w", encoding="utf-8") as f:
                f.write("2868840")
        except Exception:
            pass
    try:
        spawn_on_default_desktop(f'"{sts2_exe}"', sts2_dir)
    except Exception:
        return False

    # 等待 Godot 加载 6 个工坊模组并创建主窗口 (通常需 8~18 秒)
    for _ in range(18):
        time.sleep(1.0)
        ok_f, _ = focus_window_by_title("Slay the Spire 2")
        if ok_f:
            time.sleep(2.0)
            focus_window_by_title("Slay the Spire 2")
            return True
    return False


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
            ms = max(100, min(int(st.get("ms", 600)), 3000))
            time.sleep(ms / 1000.0)
            executed.append(f"步{idx}[等待 {ms}ms]")
        time.sleep(0.12)

    wait_s = max(0.3, min(float(wait_after_ms or 1000) / 1000.0, 3.0))
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


# ==================== 《杀戮尖塔2》后台全自动连续爬塔与复盘推进引擎 ====================

AUTOPLAY_STATE = {
    "active": False,
    "turn": 0,
    "max_turns": 25,
    "started_at": 0.0,
    "last_push_at": 0.0,
    "known_runs": set(),
    "stop_reason": "",
}


def list_profile2_history_runs():
    import glob
    runs = set()
    for sub in ("", "modded"):
        base_sub = os.path.join(STS2_STEAM_DIR, sub) if sub else STS2_STEAM_DIR
        pattern = os.path.join(base_sub, "profile2", "saves", "history", "*.run")
        for p in glob.glob(pattern):
            runs.add(os.path.abspath(p))
    return runs


def parse_finished_run_summary(run_path):
    db = load_sts2_zhs_db()
    try:
        with open(run_path, "r", encoding="utf-8") as f:
            rd = json.load(f)
        p0 = (rd.get("players") or [{}])[0]
        char_id = p0.get("character", p0.get("character_id", "")).replace("CHARACTER.", "")
        char_name = db.get("characters", {}).get(char_id, {}).get("title", char_id)
        win = rd.get("win", False)
        killed_by = rd.get("killed_by_encounter", "").replace("ENCOUNTER.", "")
        killed_by_cn = db.get("monsters", {}).get(killed_by, killed_by) or "未知怪物"
        asc = rd.get("ascension", 0)
        acts = rd.get("map_point_history", [])
        floors = sum(len(a) for a in acts)
        deck_ids = [c.get("id", "").replace("CARD.", "") for c in p0.get("deck", [])]
        deck_cn = [db.get("cards", {}).get(cid, {}).get("title", cid) for cid in deck_ids]
        relic_ids = [
            (r.get("id", "") if isinstance(r, dict) else str(r)).replace("RELIC.", "")
            for r in p0.get("relics", [])
        ]
        relic_cn = [db.get("relics", {}).get(rid, {}).get("title", rid) for rid in relic_ids]
        return (
            f"【本局结束战绩文件 {os.path.basename(run_path)}】\n"
            f"- 角色：{char_name}（进阶{asc}） | 结果：{'通关胜利！' if win else f'第{floors}层阵亡'}\n"
            f"- 致命遭遇/击败者：{killed_by_cn}({killed_by})\n"
            f"- 最终遗物与海克斯符文：{'、'.join(relic_cn)}\n"
            f"- 最终牌组({len(deck_cn)}张)：{'、'.join(deck_cn)}"
        ), killed_by_cn, char_name
    except Exception as e:
        return f"解析结束战绩失败: {e}", "未知怪物", "未知角色"


def start_sts2_autoplay(max_turns=25):
    launch_sts2_on_interactive_desktop()
    AUTOPLAY_STATE["active"] = True
    AUTOPLAY_STATE["turn"] = 0
    AUTOPLAY_STATE["max_turns"] = max(3, min(int(max_turns or 25), 50))
    AUTOPLAY_STATE["started_at"] = time.time()
    AUTOPLAY_STATE["last_push_at"] = time.time() - 10.0
    AUTOPLAY_STATE["known_runs"] = list_profile2_history_runs()
    AUTOPLAY_STATE["stop_reason"] = ""
    return (
        f"已开启《杀戮尖塔2》后台全自动连续爬塔循环（最大连续轮数：{AUTOPLAY_STATE['max_turns']}轮，本轮结束后将自动一回合接一回合连续推进，直到通关或阵亡复盘；"
        "如需中途停止，可在群里喊停调用 mode=\"sts2_autoplay_stop\"，或将本机鼠标移至屏幕最左上角(0,0)紧急刹车）。\n\n"
        + get_sts2_live_report()
    )


def stop_sts2_autoplay(reason="主动停止"):
    was_active = AUTOPLAY_STATE["active"]
    AUTOPLAY_STATE["active"] = False
    AUTOPLAY_STATE["stop_reason"] = reason
    return f"已停止《杀戮尖塔2》后台全自动连续爬塔循环（原因：{reason}，此前状态：{'运行中' if was_active else '未运行'}）。"


def inject_cloud_autoplay_turn(turn_text, img_b64):
    """通过本机 18081 隧道检查云端空闲状态，并复用 active_ssh_transport 上传 latest_screen.jpg 及注入下一回合事件"""
    import urllib.request
    try:
        # 1. 直接通过本机 127.0.0.1:18081 正向隧道查询云端机器人空闲状态（零新建SSH开销）
        req = urllib.request.Request(
            f"http://127.0.0.1:{CONSOLE_PORT}/api/v1/status",
            headers={"Authorization": "Bearer gemini-qq-bot-admin-token-2026-secret"},
        )
        with urllib.request.urlopen(req, timeout=5) as resp:
            st = json.loads(resp.read().decode("utf-8", "ignore"))

        if not st.get("health", {}).get("production_ready"):
            return False, "not_ready"
        if st.get("pending_messages", 0) > 0:
            return False, "pending_messages"
        lanes = st.get("lanes") or []
        for ln in lanes:
            if ln.get("running") or ln.get("queued", 0) > 0:
                return False, "lane_busy"
        rec_calls = st.get("recent_calls") or []
        now_ts = int(time.time())
        if rec_calls:
            c0 = rec_calls[0]
            if c0.get("state") == "running":
                return False, "call_running"
            fin = c0.get("finished_at") or 0
            if now_ts - fin < 6:
                return False, "cooldown"

        # 2. 复用已有的 active_ssh_transport 打开一个会话，一次性写入 latest_screen.jpg 并插入 pending_events
        with transport_lock:
            tr = active_ssh_transport
        if tr is None or not tr.is_active():
            return False, "ssh_transport_inactive"

        msg_id = f"sts2_auto_{now_ts}_{AUTOPLAY_STATE['turn']}"
        incoming = {
            "bot_id": "3644193154",
            "group_id": "1077671339",
            "message_id": msg_id,
            "sender_id": "2895885973",
            "speaker_ref": "u1",
            "speaker_role": "owner",
            "display_name": "濤",
            "known_alias": None,
            "received_at": now_ts,
            "segments": [
                {"kind": "at_bot"},
                {"kind": "text", "text": f" {turn_text}"},
            ],
            "text": f"@小简 {turn_text}",
            "mentions_bot": True,
            "quote_of": None,
        }
        payload_b64 = base64.b64encode(json.dumps(incoming, ensure_ascii=False).encode("utf-8")).decode("ascii")
        text_b64 = base64.b64encode(f"@小简 {turn_text}".encode("utf-8")).decode("ascii")
        img_str = img_b64 or ""

        remote_py = f"""
import sqlite3, base64
img_b64 = '{img_str}'
if img_b64:
    with open('/opt/gemini-qq-bot/data/agy-projects/1077671339/latest_screen.jpg', 'wb') as f:
        f.write(base64.b64decode(img_b64))
conn = sqlite3.connect('/opt/gemini-qq-bot/data/bot.sqlite3', timeout=10)
cur = conn.cursor()
payload = base64.b64decode('{payload_b64}').decode('utf-8')
txt = base64.b64decode('{text_b64}').decode('utf-8')
cur.execute(
    "INSERT OR IGNORE INTO group_messages (bot_id, group_id, message_id, sender_id, display_name, speaker_role, text, has_image, created_at) VALUES (?, ?, ?, ?, ?, ?, ?, 0, ?)",
    ('3644193154', '1077671339', '{msg_id}', '2895885973', '濤', 'owner', txt, {now_ts})
)
cur.execute(
    "INSERT OR IGNORE INTO pending_events (bot_id, group_id, message_id, sender_id, received_at, normalized_payload, image_refs) VALUES (?, ?, ?, ?, ?, ?, '[]')",
    ('3644193154', '1077671339', '{msg_id}', '2895885973', {now_ts}, payload)
)
conn.commit()
conn.close()
print('INJECTED_OK')
"""
        chan = tr.open_session()
        chan.settimeout(12)
        chan.exec_command("python3 -")
        chan.sendall(remote_py.encode("utf-8"))
        chan.shutdown_write()
        res_out = b""
        while True:
            chunk = chan.recv(4096)
            if not chunk:
                break
            res_out += chunk
        chan.close()
        out_str = res_out.decode("utf-8", "ignore")
        return ("INJECTED_OK" in out_str), out_str.strip()
    except Exception as e:
        return False, str(e)


def sts2_autoplay_daemon_loop():
    """后台全自动连续爬塔推进守护线程"""
    while True:
        time.sleep(4.0)
        if not AUTOPLAY_STATE["active"]:
            continue
        try:
            attach_input_desktop()
            # 1. 紧急物理刹车：鼠标移到最左上角 (0, 0)
            pt = ctypes.wintypes.POINT()
            ctypes.windll.user32.GetCursorPos(ctypes.byref(pt))
            if pt.x <= 15 and pt.y <= 15:
                stop_sts2_autoplay("检测到本机鼠标移至屏幕左上角(0,0)紧急刹车区")
                continue

            # 2. 检查窗口是否被关闭
            now = time.time()
            if now - AUTOPLAY_STATE["started_at"] > 30.0:
                vis = [w.lower() for w in list_visible_windows()]
                if not any("slay the spire" in w or "杀戮尖塔" in w for w in vis):
                    stop_sts2_autoplay("检测到《杀戮尖塔2》窗口已关闭")
                    continue

            # 3. 距离上次推进至少间隔 15 秒
            if now - AUTOPLAY_STATE["last_push_at"] < 15.0:
                continue

            # 4. 检查是否产生了新的结束战绩文件 (.run)，若是则触发终局死因复盘并停止循环
            cur_runs = list_profile2_history_runs()
            new_runs = sorted(list(cur_runs - AUTOPLAY_STATE["known_runs"]))
            if new_runs:
                latest_run = new_runs[-1]
                AUTOPLAY_STATE["known_runs"] = cur_runs
                focus_window_by_title("Slay the Spire 2")
                img_b64 = capture_screenshot_b64(draw_grid=True)
                run_summary, killed_by_cn, char_name = parse_finished_run_summary(latest_run)
                rec_logs = "\n  ".join(get_sts2_recent_logs(16))
                review_prompt = (
                    f"【《杀戮尖塔2》自动爬塔终局复盘触发】本局游戏已结束！\n{run_summary}\n"
                    f"- 死前/终局最后战斗日志：\n  {rec_logs}\n"
                    f"请立即执行以下收尾动作：\n"
                    f"1. 调用 local_pc_control(mode=\"sts2_review\", character=\"{char_name}\", enemy_name=\"{killed_by_cn}\", death_cause=\"结合日志与卡组分析具体死因\", iron_rule=\"提炼下次遇到该怪物的具体避坑铁律\", run_summary=\"本局完整复盘总结\", send_to_group=true) 将本局教训永久写入经验库并将结算画面发群；\n"
                    f"2. 在群里向主人汇报本局爬塔到达的层数、死因诊断以及新记下的避坑铁律！"
                )
                ok_inj, info = inject_cloud_autoplay_turn(review_prompt, img_b64)
                if ok_inj:
                    stop_sts2_autoplay(f"本局已结束并触发终局复盘({os.path.basename(latest_run)})")
                continue

            # 5. 检查轮数上限
            if AUTOPLAY_STATE["turn"] >= AUTOPLAY_STATE["max_turns"]:
                stop_sts2_autoplay(f"已达到单次自动连续爬塔上限({AUTOPLAY_STATE['max_turns']}轮)")
                continue

            # 6. 置顶游戏窗口并推进下一回合（无需每一步都截图，直接传后台存档与日志数据）
            focus_window_by_title("Slay the Spire 2")
            img_b64 = ""
            live_rep = get_sts2_live_report()
            next_turn = AUTOPLAY_STATE["turn"] + 1
            turn_prompt = (
                f"【《杀戮尖塔2》全自动连续爬塔推进 第{next_turn}/{AUTOPLAY_STATE['max_turns']}轮（免截图极速模式）】\n"
                f"{live_rep}\n"
                f"请立即执行本回合操作：\n"
                f"1. 不用每一步都截图！请优先根据上方后台实时存档（角色、血量、牌组、遗物、海克斯符文）和 godot.log 最新战斗日志，直接调用 local_pc_control(mode=\"sts2_batch\", send_to_group=false, steps=[...]) 一次性执行本回合整套动作（常用坐标：主菜单『单人模式』x=405,y=500；选人界面『铁甲战士』x=220,y=660、『储君』x=720,y=660、『亡灵契约师』x=600,y=660，右下角『启程/开始』x=1120,y=680；先古对话点击底部按钮x=640,y=620；海克斯符文三选一卡片分别在x=360,y=420 / x=640,y=420 / x=920,y=420；战斗中按数字键1-9选牌后点击目标怪物x=920,y=420或屏幕中央x=640,y=400，最后按 key=\"e\" 结束回合）；\n"
                f"2. 仅当你根据日志完全无法判断当前停留在什么特殊弹窗/地图分叉时，才按需调用一次 mode=\"screenshot\", send_to_group=false 查看画面；\n"
                f"3. 除非本局打完结算或主人主动询问，中间爬塔回合请一律保持群聊静默(silent)，不要每一步都往群里发图或发消息。"
            )
            ok_inj, info = inject_cloud_autoplay_turn(turn_prompt, img_b64)
            if ok_inj:
                AUTOPLAY_STATE["turn"] = next_turn
                AUTOPLAY_STATE["last_push_at"] = time.time()
                print(f"[STS2-AUTOPLAY] 已成功推进第 {next_turn}/{AUTOPLAY_STATE['max_turns']} 轮", flush=True)
            else:
                print(f"[STS2-AUTOPLAY] 等待推进第 {next_turn} 轮 ({info})", flush=True)
        except Exception as e:
            print(f"[STS2-AUTOPLAY] 循环异常: {e}", flush=True)


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
        action_note = (
            "已读取本地电脑状态（本机已安装《杀戮尖塔2 / Slay the Spire 2》，已锁定2号练习存档槽位并内置官方中文词典+海克斯大乱斗等6个工坊模组知识库；"
            "可直接调用 mode=\"open_app\", app_name=\"杀戮尖塔2\" 启动，并用 mode=\"sts2_status\" / \"sts2_batch\" / \"sts2_review\" 游玩与复盘）"
        )
        ok = True
        err_msg = None

        if mode == "open_app":
            app_arg = str(req.get("app_name") or req.get("window_title") or "")
            ok, msg = open_app_or_url(app_arg)
            if ok:
                action_note = msg
                if any(k in app_arg.lower() for k in ("杀戮尖塔", "尖塔", "sts2", "slay the spire")) and req.get("autoplay") is not False:
                    if not AUTOPLAY_STATE["active"]:
                        start_sts2_autoplay(req.get("max_turns", 25))
                        action_note += "\n（已同步自动挂起后台连续爬塔推进引擎，本轮结束后将自动一回合接一回合连续推进！）"
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
            launch_sts2_on_interactive_desktop()
            action_note = get_sts2_live_report()
            if req.get("autoplay") is not False and not AUTOPLAY_STATE["active"]:
                start_sts2_autoplay(req.get("max_turns", 25))
                action_note += "\n（已同步自动挂起后台连续爬塔推进引擎，本轮结束后将自动一回合接一回合连续推进！）"
        elif mode == "sts2_batch":
            launch_sts2_on_interactive_desktop()
            ok, msg = execute_sts2_batch(
                req.get("steps", []),
                req.get("wait_after_ms", 2200),
            )
            if ok:
                action_note = msg
                if AUTOPLAY_STATE["active"]:
                    AUTOPLAY_STATE["last_push_at"] = time.time()
            else:
                err_msg = msg
        elif mode == "sts2_review":
            ok, msg = update_sts2_review(req)
            if ok:
                action_note = msg
            else:
                err_msg = msg
        elif mode == "sts2_autoplay_start":
            action_note = start_sts2_autoplay(req.get("max_turns", 25))
        elif mode == "sts2_autoplay_stop":
            action_note = stop_sts2_autoplay(str(req.get("reason") or "主人或机器人主动停止"))

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
        # 不用每一步都截图：仅在显式调用 mode="screenshot" 或传入 take_screenshot=True 时才截屏
        if ok and (mode == "screenshot" or req.get("take_screenshot") is True):
            try:
                img_b64 = capture_screenshot_b64()
            except Exception as e:
                action_note += f"（截屏异常: {e}）"

        installed_apps = []
        if mode == "status":
            all_keys = list(scan_installed_shortcuts().keys())
            priority_words = (
                "杀戮尖塔2",
                "slay the spire 2",
                "杀戮尖塔",
                "slay the spire",
                "星穹铁道",
                "stardew",
                "silksong",
                "terraria",
                "饥荒",
                "暖雪",
                "weixin",
                "微信",
                "哔哩哔哩",
                "网易云",
                "steam",
                "chrome",
            )
            top_apps = [k for k in all_keys if any(w in k.lower() for w in priority_words)]
            rest_apps = sorted([k for k in all_keys if k not in top_apps])
            installed_apps = (top_apps + rest_apps)[:60]

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
        try:
            self.send_response(200)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(payload)))
            self.send_header("Connection", "close")
            self.end_headers()
            self.wfile.write(payload)
            self.wfile.flush()
        except (ConnectionAbortedError, ConnectionResetError, BrokenPipeError, OSError):
            pass


def pipe_sockets(chan, sock):
    """双向管道转发：使用写半关闭(shutdown_write)防止一侧读完EOF时提前把另一侧正在回传的HTTP响应掐断"""
    def forward(src, dst, is_dst_chan):
        try:
            while True:
                data = src.recv(32768)
                if not data:
                    break
                dst.sendall(data)
        except Exception:
            pass
        finally:
            try:
                if is_dst_chan:
                    dst.shutdown_write()
                else:
                    dst.shutdown(socket.SHUT_WR)
            except Exception:
                pass

    def supervisor():
        t1 = threading.Thread(target=forward, args=(chan, sock, False), daemon=True)
        t2 = threading.Thread(target=forward, args=(sock, chan, True), daemon=True)
        t1.start()
        t2.start()
        t1.join(timeout=60)
        t2.join(timeout=60)
        try:
            sock.close()
        except Exception:
            pass
        try:
            chan.close()
        except Exception:
            pass

    threading.Thread(target=supervisor, daemon=True).start()


def handle_reverse_channel(chan):
    try:
        sock = socket.create_connection((BRIDGE_HOST, BRIDGE_PORT), timeout=8)
        sock.setsockopt(socket.IPPROTO_TCP, socket.TCP_NODELAY, 1)
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
                client_sock.setsockopt(socket.IPPROTO_TCP, socket.TCP_NODELAY, 1)
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
            # 创建带 Windows 原生 TCP KeepAlive 与 TCP_NODELAY 的底层直连 Socket，防止被代理或 NAT 掐断
            raw_sock = socket.create_connection((CLOUD_HOST, CLOUD_PORT), timeout=15)
            raw_sock.setsockopt(socket.IPPROTO_TCP, socket.TCP_NODELAY, 1)
            raw_sock.setsockopt(socket.SOL_SOCKET, socket.SO_KEEPALIVE, 1)
            if hasattr(raw_sock, "ioctl") and hasattr(socket, "SIO_KEEPALIVE_VALS"):
                # (开启=1, 10秒空闲发心跳=10000ms, 每3秒重试=3000ms)
                raw_sock.ioctl(socket.SIO_KEEPALIVE_VALS, (1, 10000, 3000))

            client = paramiko.SSHClient()
            client.set_missing_host_key_policy(paramiko.AutoAddPolicy())
            client.connect(
                CLOUD_HOST,
                CLOUD_PORT,
                CLOUD_USER,
                CLOUD_PASS,
                timeout=15,
                banner_timeout=20,
                auth_timeout=15,
                sock=raw_sock,
            )
            _, out, _ = client.exec_command(f"fuser -k {BRIDGE_PORT}/tcp 2>/dev/null || true")
            out.channel.recv_exit_status()
            time.sleep(0.25)

            transport = client.get_transport()
            transport.set_keepalive(12)
            with transport_lock:
                active_ssh_transport = transport
            transport.request_port_forward("127.0.0.1", BRIDGE_PORT)
            print(f"[BRIDGE] 已成功挂载至云端小简 (云端 127.0.0.1:{BRIDGE_PORT} -> 本机 {BRIDGE_HOST}:{BRIDGE_PORT})", flush=True)
            while transport.is_active():
                chan = transport.accept(timeout=5)
                if chan is not None:
                    threading.Thread(target=handle_reverse_channel, args=(chan,), daemon=True).start()
        except Exception as e:
            print(f"[BRIDGE] 反向隧道断开或重连中: {e}", flush=True)
        time.sleep(2)


def main():
    try:
        server = ThreadingHTTPServer((BRIDGE_HOST, BRIDGE_PORT), BridgeHandler)
    except OSError:
        return
    threading.Thread(target=server.serve_forever, daemon=True).start()
    threading.Thread(target=sts2_autoplay_daemon_loop, daemon=True).start()
    start_console_forward_listener()
    print(f"[BRIDGE] 本地电脑全功能控制服务已启动于 http://{BRIDGE_HOST}:{BRIDGE_PORT}")
    reverse_tunnel_loop()


if __name__ == "__main__":
    main()
