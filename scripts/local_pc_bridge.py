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
    "杀戮尖塔": "Slay the Spire",
    "杀戮尖塔1": "Slay the Spire",
    "杀戮尖塔2": "Slay the Spire 2",
    "尖塔": "Slay the Spire",
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
