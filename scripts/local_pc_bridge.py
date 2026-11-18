#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""
小简本地Windows电脑热插拔桥接节点 (Local PC Plug-and-Play Bridge Node)
功能：
1. 在本机 127.0.0.1:18095 启动轻量级控制接口（屏幕截图、窗口列表、鼠标点击、键盘按键、切换窗口焦点）；
2. 自动绑定当前线程至 Windows 交互式输入桌面 (OpenInputDesktop + SetThreadDesktop)，支持后台服务/沙箱无黑框截屏；
3. 自动清理云端残留端口占用并建立 SSH 反向隧道 (云端 127.0.0.1:18095 -> 本机 127.0.0.1:18095)；
4. 自动建立本地管理台正向隧道 (本机 127.0.0.1:18081 -> 云端 127.0.0.1:18081)。
"""

import base64
import ctypes
import ctypes.wintypes
import io
import json
import socket
import threading
import time
from http.server import BaseHTTPRequestHandler, HTTPServer

import paramiko
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

VK_MAP = {
    "space": 0x20,
    "enter": 0x0D,
    "return": 0x0D,
    "esc": 0x1B,
    "escape": 0x1B,
    "tab": 0x09,
    "backspace": 0x08,
    "left": 0x25,
    "up": 0x26,
    "right": 0x27,
    "down": 0x28,
}

active_ssh_transport = None
transport_lock = threading.Lock()


def attach_input_desktop():
    """将当前工作线程绑定到 Windows 活动交互桌面 (WinSta0\\Default)，解决后台进程截屏报 screen grab failed 的问题"""
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
    return titles[:15]


def focus_window_by_title(keyword):
    attach_input_desktop()
    kw = keyword.lower().strip()
    if not kw:
        return False, "未提供窗口标题关键词"
    found = []

    def enum_cb(hwnd, _):
        if win32gui.IsWindowVisible(hwnd):
            t = win32gui.GetWindowText(hwnd).strip()
            if kw in t.lower():
                found.append((hwnd, t))

    win32gui.EnumWindows(enum_cb, None)
    if not found:
        return False, f"未找到包含「{keyword}」的窗口"
    hwnd, title = found[0]
    try:
        win32gui.ShowWindow(hwnd, win32con.SW_RESTORE)
        win32gui.SetForegroundWindow(hwnd)
        time.sleep(0.4)
        return True, f"已切换到窗口：{title}"
    except Exception as e:
        return False, f"切换窗口失败: {e}"


def click_screen(x, y, button="left"):
    attach_input_desktop()
    w, h = get_screen_size()
    tx = max(0, min(int(x), w - 1))
    ty = max(0, min(int(y), h - 1))
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
    return f"已在坐标 ({tx}, {ty}) 执行鼠标 {button} 点击"


def press_key(key_name):
    attach_input_desktop()
    k = key_name.lower().strip()
    if k in VK_MAP:
        vk = VK_MAP[k]
    elif len(k) == 1 and k.isalnum():
        vk = ord(k.upper())
    else:
        return False, f"暂不支持按键名: {key_name}"
    ctypes.windll.user32.keybd_event(vk, 0, 0, 0)
    time.sleep(0.08)
    ctypes.windll.user32.keybd_event(vk, 0, 0x0002, 0)
    time.sleep(0.5)
    return True, f"已按下按键: {key_name}"


def capture_screenshot_b64():
    attach_input_desktop()
    img = ImageGrab.grab()
    pt = ctypes.wintypes.POINT()
    ctypes.windll.user32.GetCursorPos(ctypes.byref(pt))
    draw = ImageDraw.Draw(img)
    r = 8
    draw.ellipse((pt.x - r, pt.y - r, pt.x + r, pt.y + r), outline="red", width=3)

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

        if mode == "click":
            x = req.get("x", 640)
            y = req.get("y", 360)
            btn = req.get("button", "left")
            action_note = click_screen(x, y, btn)
        elif mode == "key":
            ok, msg = press_key(str(req.get("key", "space")))
            if ok:
                action_note = msg
            else:
                err_msg = msg
        elif mode == "focus_window":
            ok, msg = focus_window_by_title(str(req.get("window_title", "")))
            if ok:
                action_note = msg
            else:
                err_msg = msg
        elif mode == "screenshot":
            action_note = "已截取本地电脑实时画面"

        w, h = get_screen_size()
        img_b64 = ""
        if ok and mode in ("screenshot", "click", "key", "focus_window"):
            try:
                img_b64 = capture_screenshot_b64()
            except Exception as e:
                action_note += f"（截屏异常: {e}）"

        resp = {
            "ok": ok,
            "error": err_msg,
            "action_note": action_note,
            "active_window": get_active_window_title(),
            "screen_size": f"{w}x{h}",
            "windows": list_visible_windows(),
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
            # 先清理云端可能残留的旧 18095 转发进程，防止端口被死连接占用
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
    print(f"[BRIDGE] 本地电脑控制服务已启动于 http://{BRIDGE_HOST}:{BRIDGE_PORT}")
    reverse_tunnel_loop()


if __name__ == "__main__":
    main()
