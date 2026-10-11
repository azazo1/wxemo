"""把 emoticon 密文目录完整还原成可直接查看的图片.

流程:
1. 用密钥 AES-128-CBC 解密每个文件, 去掉 PKCS#7 填充;
2. 按格式魔数切分多图拼接容器;
3. 校验每个切片的完整性 (PNG 校验全部 chunk 的 CRC);
4. 按原目录层次写出, 并输出统计.

用法:
    python3 extract_emoticons.py <密文根目录> <输出目录> --key HEX
"""

from __future__ import annotations

import argparse
import os
import struct
import sys
import zlib
from collections import Counter

from Crypto.Cipher import AES

MAGICS = [
    (b"\x89PNG\r\n\x1a\n", "png"),
    (b"GIF87a", "gif"),
    (b"GIF89a", "gif"),
    (b"\xff\xd8\xff", "jpg"),
    (b"wxgf", "wxgf"),
    (b"BM", "bmp"),
]


def guess_format(data: bytes) -> str:
    for magic, name in MAGICS:
        if data.startswith(magic):
            return name
    if data[:4] == b"RIFF" and data[8:12] == b"WEBP":
        return "webp"
    return "bin"


def strip_padding(data: bytes) -> tuple[bytes, bool]:
    if not data:
        return data, False
    n = data[-1]
    if 1 <= n <= 16 and len(data) >= n and data[-n:] == bytes([n]) * n:
        return data[:-n], True
    return data, False


def png_is_intact(data: bytes) -> bool:
    """逐 chunk 校验 PNG 的长度与 CRC."""
    if not data.startswith(b"\x89PNG\r\n\x1a\n"):
        return False
    pos = 8
    while pos + 12 <= len(data):
        length = struct.unpack(">I", data[pos : pos + 4])[0]
        kind = data[pos + 4 : pos + 8]
        if pos + 12 + length > len(data):
            return False
        chunk = data[pos + 8 : pos + 8 + length]
        stored = struct.unpack(">I", data[pos + 8 + length : pos + 12 + length])[0]
        if zlib.crc32(kind + chunk) & 0xFFFFFFFF != stored:
            return False
        pos += 12 + length
        if kind == b"IEND":
            return pos == len(data)
    return False


def split_container(data: bytes) -> list[tuple[bytes, str]]:
    """按魔数切分多图拼接容器."""
    segments: list[list[int | str]] = []

    for magic, kind in ((b"\x89PNG\r\n\x1a\n", "png"),):
        cursor = 0
        while True:
            start = data.find(magic, cursor)
            if start < 0:
                break
            end = data.find(b"IEND", start)
            if end < 0:
                break
            end += 8
            segments.append([start, end, kind])
            cursor = end

    cursor = 0
    while True:
        start = data.find(b"GIF8", cursor)
        if start < 0:
            break
        end = data.find(b"\x3b", start)
        if end < 0:
            break
        segments.append([start, end + 1, "gif"])
        cursor = end + 1

    cursor = 0
    while True:
        start = data.find(b"\xff\xd8\xff", cursor)
        if start < 0:
            break
        end = data.find(b"\xff\xd9", start)
        if end < 0:
            break
        segments.append([start, end + 2, "jpg"])
        cursor = end + 2

    if not segments:
        return [(data, guess_format(data))]

    segments.sort(key=lambda item: item[0])
    out: list[tuple[bytes, str]] = []
    for index, (start, end, kind) in enumerate(segments):
        if end <= start:
            continue
        out.append((data[start:end], str(kind)))
    return out


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("src")
    parser.add_argument("dst")
    parser.add_argument("--key", required=True, help="16 字节密钥的十六进制")
    args = parser.parse_args()

    key = bytes.fromhex(args.key)
    if len(key) != 16:
        print("密钥必须是 16 字节", file=sys.stderr)
        return 1

    formats = Counter()
    bad_padding = 0
    bad_png = 0
    files = 0
    written = 0

    for root, _, names in os.walk(args.src):
        for name in names:
            path = os.path.join(root, name)
            rel = os.path.relpath(path, args.src)
            with open(path, "rb") as handle:
                blob = handle.read()
            if not blob or len(blob) % 16:
                print(f"跳过 {rel}: 长度 {len(blob)} 不是 16 的倍数", file=sys.stderr)
                continue

            plain = AES.new(key, AES.MODE_CBC, key).decrypt(blob)
            body, padding_ok = strip_padding(plain)
            if not padding_ok:
                bad_padding += 1
            files += 1

            parts = split_container(body)
            for index, (chunk, kind) in enumerate(parts):
                if kind == "png" and not png_is_intact(chunk):
                    bad_png += 1
                formats[kind] += 1
                suffix = "" if len(parts) == 1 else f".{index:03d}"
                out = os.path.join(args.dst, f"{rel}{suffix}.{kind}")
                os.makedirs(os.path.dirname(out), exist_ok=True)
                with open(out, "wb") as handle:
                    handle.write(chunk)
                written += 1

    print(f"解密文件 {files} 个, 写出图片 {written} 张")
    print(f"格式分布: {dict(formats.most_common())}")
    print(f"填充异常 {bad_padding} 个, PNG 结构异常 {bad_png} 个")
    return 0


if __name__ == "__main__":
    sys.exit(main())
