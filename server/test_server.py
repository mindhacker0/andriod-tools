#!/usr/bin/env python3
"""phone-tool 的参考服务端（仅用 Python 标准库，零依赖）。

接收 phone-tool 上传的 multipart/form-data：

    POST /api/photo/upload
      - metadata : JSON 字符串（照片全部信息）
      - file     : 照片二进制（可选）

行为：
    1. 照片文件保存到 server/uploads/<photo.relative_path>
    2. 元数据逐行追加到 server/metadata.jsonl
    3. 每个请求打印一行摘要

启动：
    python server/test_server.py [--host 127.0.0.1] [--port 8080]
"""

from __future__ import annotations

import argparse
import json
import sys
from email import message_from_bytes
from email.policy import default as email_policy
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path

BASE_DIR = Path(__file__).resolve().parent
UPLOAD_DIR = BASE_DIR / "uploads"
METADATA_FILE = BASE_DIR / "metadata.jsonl"


def parse_multipart(content_type: str, body: bytes):
    """用 email 模块把 multipart 解析成 {字段名: (文件名, 字节)}。

    返回 (fields, files)，fields 是文本字段，files 是文件字段。
    """
    raw = (
        f"Content-Type: {content_type}\r\nMIME-Version: 1.0\r\n\r\n".encode()
        + body
    )
    msg = message_from_bytes(raw, policy=email_policy)

    fields: dict[str, str] = {}
    files: dict[str, tuple[str, bytes]] = {}
    if not msg.is_multipart():
        return fields, files

    for part in msg.iter_parts():
        name = part.get_param("name", header="content-disposition")
        if not name:
            continue
        payload = part.get_payload(decode=True) or b""
        filename = part.get_filename()
        if filename:
            files[name] = (filename, payload)
        else:
            charset = part.get_content_charset() or "utf-8"
            fields[name] = payload.decode(charset, errors="replace")
    return fields, files


def safe_join(root: Path, relative: str) -> Path:
    """把相对路径安全地拼到 root 下，避免 ../ 逃逸。"""
    target = (root / relative.replace("\\", "/")).resolve()
    if not str(target).startswith(str(root.resolve())):
        raise ValueError(f"非法的相对路径: {relative}")
    return target


class Handler(BaseHTTPRequestHandler):
    server_version = "phone-tool-test-server"

    def log_message(self, fmt: str, *args) -> None:  # 关掉默认的访问日志，输出更干净
        pass

    def _json(self, status: int, payload: dict) -> None:
        data = json.dumps(payload, ensure_ascii=False).encode()
        self.send_response(status)
        self.send_header("Content-Type", "application/json; charset=utf-8")
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)

    def do_POST(self) -> None:  # noqa: N802
        # 兼容新默认路径 /api/photo/upload，同时保留旧路径 /api/photos/upload
        if not (
            self.path.startswith("/api/photo/upload")
            or self.path.startswith("/api/photos/upload")
        ):
            self._json(404, {"error": f"unknown path {self.path}"})
            return

        length = int(self.headers.get("Content-Length") or 0)
        body = self.rfile.read(length) if length else b""
        content_type = self.headers.get("Content-Type", "")

        try:
            fields, files = parse_multipart(content_type, body)
        except Exception as exc:  # noqa: BLE001
            self._json(400, {"error": f"multipart 解析失败: {exc}"})
            return

        raw_metadata = fields.get("metadata")
        if not raw_metadata:
            self._json(400, {"error": "缺少 metadata 字段"})
            return
        try:
            metadata = json.loads(raw_metadata)
        except json.JSONDecodeError as exc:
            self._json(400, {"error": f"metadata 不是合法 JSON: {exc}"})
            return

        photo = metadata.get("photo") or {}
        relative = photo.get("relative_path") or photo.get("filename") or "unknown.jpg"
        request_id = metadata.get("request_id", "-")
        storage_description = (metadata.get("storage") or {}).get("description", "-")
        token_ok = bool(self.headers.get("Authorization"))

        # EXIF（读不到时是 null）
        exif = photo.get("exif") or {}
        highlights = exif.get("highlights") or {}
        if exif:
            camera = " ".join(
                str(highlights[k]) for k in ("Make", "Model") if highlights.get(k)
            ) or "未知"
            gps = exif.get("gps")
            exif_note = f"EXIF {exif.get('field_count', 0)} 项 · {camera}"
            if gps:
                exif_note += f" · GPS {gps['latitude']:.6f},{gps['longitude']:.6f}"
        else:
            exif_note = "无 EXIF"

        saved_path = None
        if "file" in files:
            original_name, payload = files["file"]
            try:
                target = safe_join(UPLOAD_DIR, relative)
            except ValueError:
                target = UPLOAD_DIR / original_name
            target.parent.mkdir(parents=True, exist_ok=True)
            target.write_bytes(payload)
            saved_path = target

        record = {
            "request_id": request_id,
            "received_authorization": token_ok,
            "file_saved_to": str(saved_path) if saved_path else None,
            "metadata": metadata,
        }
        METADATA_FILE.parent.mkdir(parents=True, exist_ok=True)
        with METADATA_FILE.open("a", encoding="utf-8") as fh:
            fh.write(json.dumps(record, ensure_ascii=False) + "\n")

        size_note = (
            f"{len(files['file'][1])} 字节" if "file" in files else "仅元数据"
        )
        print(
            f"[{self.log_date_time_string()}] ✅ {photo.get('filename', '?')} "
            f"({size_note}) · 存储分区={storage_description} · {exif_note} · id={request_id}"
            + (f" → {saved_path}" if saved_path else ""),
            flush=True,
        )

        self._json(
            200,
            {
                "status": "ok",
                "request_id": request_id,
                "file_saved": saved_path is not None,
                "filename": photo.get("filename"),
                "exif_fields": exif.get("field_count", 0) if exif else 0,
            },
        )


def main() -> int:
    parser = argparse.ArgumentParser(description="phone-tool 参考服务端")
    parser.add_argument("--host", default="127.0.0.1")
    parser.add_argument("--port", type=int, default=8080)
    args = parser.parse_args()

    UPLOAD_DIR.mkdir(parents=True, exist_ok=True)
    server = ThreadingHTTPServer((args.host, args.port), Handler)
    print(f"phone-tool 参考服务端已启动: http://{args.host}:{args.port}/api/photo/upload")
    print(f"  照片保存到 : {UPLOAD_DIR}")
    print(f"  元数据追加到: {METADATA_FILE}")
    print("  Ctrl+C 退出")
    try:
        server.serve_forever()
    except KeyboardInterrupt:
        print("\n已退出")
    return 0


if __name__ == "__main__":
    sys.exit(main())
