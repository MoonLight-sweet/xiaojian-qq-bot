#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""
小简群聊语音与短视频轻量识别预处理工具 (兼容 Python 3.6+，零第三方依赖)
底层复用两个成熟开源静态组件：
1. FFmpeg 静态预编译版 (/usr/local/bin/ffmpeg, /usr/local/bin/ffprobe)
2. k2-fsa/sherpa-onnx 离线 C++ 引擎 + SenseVoiceSmall (int8 量化版，中文/多语种+标点+情绪)

支持两种模式：
  python3 media_light_recognize.py voice --file <path_or_empty> --url <url_or_empty>
  python3 media_light_recognize.py video --file <path_or_empty> --url <url_or_empty> --out-grid <grid.jpg>
"""

import argparse
import json
import os
import re
import shutil
import subprocess
import sys
import tempfile
import urllib.request

FFMPEG = "/usr/local/bin/ffmpeg"
FFPROBE = "/usr/local/bin/ffprobe"
SHERPA_BIN = "/opt/gemini-qq-bot/tools/asr/bin/sherpa-onnx-offline"
SHERPA_MODEL_DIR = "/opt/gemini-qq-bot/tools/asr/sense-voice"
MAX_DOWNLOAD_BYTES = 35 * 1024 * 1024  # 35MB 上限，保护云端内存
MAX_AUDIO_SECONDS = 60


def download_to(url, dest_path):
    req = urllib.request.Request(
        url,
        headers={
            "User-Agent": "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36",
        },
    )
    total = 0
    with urllib.request.urlopen(req, timeout=25) as resp, open(dest_path, "wb") as f:
        while True:
            chunk = resp.read(65536)
            if not chunk:
                break
            total += len(chunk)
            if total > MAX_DOWNLOAD_BYTES:
                raise RuntimeError("媒体文件超过35MB轻量处理上限")
            f.write(chunk)
    return total


def resolve_input_media(file_arg, url_arg, tmp_dir, default_ext):
    if file_arg:
        clean = file_arg.replace("file://", "")
        if os.path.isfile(clean):
            return clean
    if url_arg and (url_arg.startswith("http://") or url_arg.startswith("https://")):
        dst = os.path.join(tmp_dir, "input" + default_ext)
        download_to(url_arg, dst)
        return dst
    raise RuntimeError("未找到可读的本地文件或有效下载链接")


def probe_duration(path):
    try:
        r = subprocess.run(
            [
                FFPROBE,
                "-v", "error",
                "-show_entries", "format=duration",
                "-of", "default=noprint_wrappers=1:nokey=1",
                path,
            ],
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            timeout=10,
        )
        val = r.stdout.decode("utf-8", "ignore").strip()
        return round(float(val), 1) if val else 0.0
    except Exception:
        return 0.0


def run_sherpa_asr(wav_path):
    model_path = os.path.join(SHERPA_MODEL_DIR, "model.int8.onnx")
    tokens_path = os.path.join(SHERPA_MODEL_DIR, "tokens.txt")
    if not (os.path.isfile(SHERPA_BIN) and os.path.isfile(model_path) and os.path.isfile(tokens_path)):
        return "", "", ""

    r = subprocess.run(
        [
            SHERPA_BIN,
            "--sense-voice-model=" + model_path,
            "--tokens=" + tokens_path,
            "--sense-voice-use-itn=1",
            "--num-threads=2",
            wav_path,
        ],
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        timeout=35,
    )
    combined = (
        r.stdout.decode("utf-8", "ignore") + "\n" + r.stderr.decode("utf-8", "ignore")
    )
    # sherpa-onnx-offline 输出中包含 JSON 对象行 {"text": "...", "emotion": "...", "event": "..."}
    text = ""
    emotion = ""
    event = ""
    for line in combined.splitlines():
        line = line.strip()
        if line.startswith("{") and '"text"' in line:
            try:
                obj = json.loads(line)
                text = str(obj.get("text") or "").strip()
                emotion = str(obj.get("emotion") or "").strip()
                event = str(obj.get("event") or "").strip()
            except Exception:
                pass
    # 清理 SenseVoice 可能残留的 <|zh|><|NEUTRAL|> 标签
    text = re.sub(r"<\|[^|>]+\|>", "", text).strip()
    emotion = re.sub(r"<\|([^|>]+)\|>", r"\1", emotion).strip()
    event = re.sub(r"<\|([^|>]+)\|>", r"\1", event).strip()
    return text, emotion, event


def handle_voice(args, tmp_dir):
    src = resolve_input_media(args.file, args.url, tmp_dir, ".audio")
    wav_path = os.path.join(tmp_dir, "voice_16k.wav")
    r = subprocess.run(
        [
            FFMPEG,
            "-y",
            "-i", src,
            "-t", str(MAX_AUDIO_SECONDS),
            "-ar", "16000",
            "-ac", "1",
            "-c:a", "pcm_s16le",
            wav_path,
        ],
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        timeout=20,
    )
    if r.returncode != 0 or not os.path.isfile(wav_path):
        err_tail = r.stderr.decode("utf-8", "ignore")[-200:]
        return {"ok": False, "error": "音频转码失败: " + err_tail}

    dur = probe_duration(wav_path)
    text, emotion, event = run_sherpa_asr(wav_path)
    return {
        "ok": True,
        "kind": "voice",
        "duration": dur,
        "text": text,
        "emotion": emotion,
        "event": event,
    }


def handle_video(args, tmp_dir):
    src = resolve_input_media(args.file, args.url, tmp_dir, ".mp4")
    dur = probe_duration(src)

    # 1. 尝试提取音轨并做轻量语音转写
    transcript = ""
    emotion = ""
    has_audio = False
    wav_path = os.path.join(tmp_dir, "video_16k.wav")
    r_aud = subprocess.run(
        [
            FFMPEG,
            "-y",
            "-i", src,
            "-t", str(MAX_AUDIO_SECONDS),
            "-vn",
            "-ar", "16000",
            "-ac", "1",
            "-c:a", "pcm_s16le",
            wav_path,
        ],
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        timeout=25,
    )
    if r_aud.returncode == 0 and os.path.isfile(wav_path) and os.path.getsize(wav_path) > 1000:
        has_audio = True
        transcript, emotion, _ = run_sherpa_asr(wav_path)

    # 2. 均匀抽取 4 帧关键画面拼成 2x2 四宫格缩略图（960x540），供现有前置视觉模型一次看懂全片
    grid_out = args.out_grid or os.path.join(tmp_dir, "grid.jpg")
    os.makedirs(os.path.dirname(os.path.abspath(grid_out)), exist_ok=True)
    # 按视频总时长计算抽帧速率（若时长未知则默认按每秒0.5帧抽4帧）
    safe_dur = max(dur, 2.0)
    fps_val = max(0.05, min(2.0, 4.0 / safe_dur))
    vf = "fps={:.4f},scale=480:270:force_original_aspect_ratio=decrease,pad=480:270:(ow-iw)/2:(oh-ih)/2:black,tile=2x2".format(fps_val)
    r_vid = subprocess.run(
        [
            FFMPEG,
            "-y",
            "-i", src,
            "-frames:v", "1",
            "-vf", vf,
            "-q:v", "4",
            grid_out,
        ],
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        timeout=25,
    )
    grid_ok = (r_vid.returncode == 0 and os.path.isfile(grid_out) and os.path.getsize(grid_out) > 500)
    if not grid_ok:
        # 兜底：若短视频极短(<1秒)导致 tile=2x2 凑不够4帧，则直接抽第1帧
        r_fb = subprocess.run(
            [
                FFMPEG,
                "-y",
                "-i", src,
                "-frames:v", "1",
                "-vf", "scale=720:-2",
                "-q:v", "4",
                grid_out,
            ],
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            timeout=15,
        )
        grid_ok = (r_fb.returncode == 0 and os.path.isfile(grid_out) and os.path.getsize(grid_out) > 500)

    return {
        "ok": True,
        "kind": "video",
        "duration": dur,
        "has_audio": has_audio,
        "transcript": transcript,
        "emotion": emotion,
        "grid_path": grid_out if grid_ok else "",
    }


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("mode", choices=["voice", "video"])
    parser.add_argument("--file", default="")
    parser.add_argument("--url", default="")
    parser.add_argument("--out-grid", default="")
    args = parser.parse_args()

    tmp_dir = tempfile.mkdtemp(prefix="qq_media_")
    try:
        if args.mode == "voice":
            res = handle_voice(args, tmp_dir)
        else:
            res = handle_video(args, tmp_dir)
        print(json.dumps(res, ensure_ascii=False))
    except Exception as e:
        print(json.dumps({"ok": False, "error": str(e)}, ensure_ascii=False))
        sys.exit(1)
    finally:
        shutil.rmtree(tmp_dir, ignore_errors=True)


if __name__ == "__main__":
    main()
