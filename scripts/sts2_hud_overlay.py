#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""
小简《杀戮尖塔2》物理桌面置顶悬浮状态指示条 (winsta0\default 独立GUI进程)
- 置顶、半透明、鼠标完全穿透、不抢游戏焦点；
- 每 250ms 读取 data/sts2_hud_state.json 更新绿/黄/红指示灯与文字；
- 若超过 8 秒未收到桥接心跳，自动变红提示“已断开连接”。
"""

import ctypes
import json
import os
import time
import tkinter as tk

try:
    ctypes.windll.user32.SetProcessDPIAware()
except Exception:
    pass

BASE_DIR = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
STATE_FILE = os.path.join(BASE_DIR, "data", "sts2_hud_state.json")
PID_FILE = os.path.join(BASE_DIR, "data", "sts2_hud_overlay.pid")


def main():
    os.makedirs(os.path.dirname(STATE_FILE), exist_ok=True)
    try:
        with open(PID_FILE, "w", encoding="utf-8") as f:
            f.write(str(os.getpid()))
    except Exception:
        pass

    root = tk.Tk()
    root.title("XiaojianSTS2HUD")
    root.overrideredirect(True)
    root.attributes("-topmost", True)
    root.attributes("-alpha", 0.88)
    root.configure(bg="#0F172A")

    sw = root.winfo_screenwidth()
    w, h = 680, 38
    x = max(0, (sw - w) // 2)
    y = 8
    root.geometry(f"{w}x{h}+{x}+{y}")

    frame = tk.Frame(root, bg="#0F172A", highlightbackground="#334155", highlightthickness=1)
    frame.pack(fill="both", expand=True)

    canvas = tk.Canvas(frame, width=22, height=22, bg="#0F172A", highlightthickness=0)
    canvas.pack(side="left", padx=(14, 6), pady=7)
    dot = canvas.create_oval(4, 4, 18, 18, fill="#10B981", outline="")

    lbl = tk.Label(
        frame,
        text="小简状态条已就绪 | 等待启动《杀戮尖塔2》自动爬塔",
        fg="#F8FAFC",
        bg="#0F172A",
        font=("Microsoft YaHei UI", 10, "bold"),
        anchor="w",
    )
    lbl.pack(side="left", fill="x", expand=True, padx=(0, 14))

    root.update_idletasks()
    # 设置鼠标完全穿透 + 不抢前台焦点 + 隐藏任务栏图标
    hwnd = ctypes.windll.user32.GetParent(root.winfo_id()) or root.winfo_id()
    GWL_EXSTYLE = -20
    ex_style = ctypes.windll.user32.GetWindowLongW(hwnd, GWL_EXSTYLE)
    ctypes.windll.user32.SetWindowLongW(
        hwnd,
        GWL_EXSTYLE,
        ex_style | 0x00080000 | 0x00000020 | 0x08000000 | 0x00000080,
    )

    def tick():
        try:
            # 保持强制置顶（防止全屏游戏盖住）
            ctypes.windll.user32.SetWindowPos(hwnd, -1, x, y, w, h, 0x0010 | 0x0040)
        except Exception:
            pass

        color = "#F59E0B"
        text = "小简状态条在线 | 等待指令..."
        visible = True
        try:
            if os.path.exists(STATE_FILE):
                with open(STATE_FILE, "r", encoding="utf-8") as f:
                    st = json.load(f)
                visible = bool(st.get("visible", True))
                color = str(st.get("color") or "#10B981")
                text = str(st.get("text") or text)
                updated_at = float(st.get("updated_at") or 0)
                active = bool(st.get("active", False))
                # 如果声明运行中但超过10秒未刷新心跳，自动变红报警
                if active and updated_at > 0 and (time.time() - updated_at) > 10.0:
                    color = "#EF4444"
                    text = f"小简自动爬塔已断开（心跳超时 {int(time.time() - updated_at)}秒）"
        except Exception:
            pass

        if visible:
            if root.state() == "withdrawn":
                root.deiconify()
            canvas.itemconfig(dot, fill=color)
            lbl.config(text=text)
        else:
            if root.state() != "withdrawn":
                root.withdraw()

        root.after(250, tick)

    root.after(250, tick)
    root.mainloop()


if __name__ == "__main__":
    main()
