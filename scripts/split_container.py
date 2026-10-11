"""按魔数把一个文件切成多个独立图片.

PersistStore / ThumbStore 下的大文件是多个图片首尾相接的容器,
需要按各格式的结束标记逐个切分.

用法: python3 split_container.py <文件> [--out-dir DIR]
只打印切分结果; 给了 --out-dir 才写文件.
"""

from __future__ import annotations

import argparse
import os
import sys

#: 每种格式的 (起始魔数, 结束标记或 None 表示需要另外解析).
PNG_START = b"\x89PNG\r\n\x1a\n"
PNG_END = b"IEND"


def find_segments(data: bytes) -> list[tuple[int, int, str]]:
    """返回 [(起始, 结束, 格式)] 列表, 结束为开区间."""
    segments: list[tuple[int, int, str]] = []

    # PNG: 从魔数到 IEND 之后的 4 字节 CRC
    cursor = 0
    while True:
        start = data.find(PNG_START, cursor)
        if start < 0:
            break
        end = data.find(PNG_END, start)
        if end < 0:
            break
        end += len(PNG_END) + 4
        segments.append((start, end, "png"))
        cursor = end

    # GIF: 从 GIF8 到 0x3B 结束符
    cursor = 0
    while True:
        start = data.find(b"GIF8", cursor)
        if start < 0:
            break
        end = data.find(b"\x3b", start)
        if end < 0:
            break
        segments.append((start, end + 1, "gif"))
        cursor = end + 1

    # JPEG: 从 FFD8FF 到 FFD9
    cursor = 0
    while True:
        start = data.find(b"\xff\xd8\xff", cursor)
        if start < 0:
            break
        end = data.find(b"\xff\xd9", start)
        if end < 0:
            break
        segments.append((start, end + 2, "jpg"))
        cursor = end + 2

    # wxgf: 没有明确结束符, 只能取到下一个魔数起点
    cursor = 0
    while True:
        start = data.find(b"wxgf", cursor)
        if start < 0:
            break
        segments.append((start, -1, "wxgf"))
        cursor = start + 1

    segments.sort()
    return segments


def split(data: bytes) -> list[tuple[bytes, str]]:
    """切分并返回 [(内容, 格式)]."""
    segments = find_segments(data)
    if not segments:
        return [(data, "bin")]

    # 对没有结束标记的格式, 用下一个片段的起点作为终点.
    out: list[tuple[bytes, str]] = []
    for index, (start, end, kind) in enumerate(segments):
        if end < 0:
            end = segments[index + 1][0] if index + 1 < len(segments) else len(data)
        if end <= start:
            continue
        out.append((data[start:end], kind))
    return out


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("path")
    parser.add_argument("--out-dir")
    args = parser.parse_args()

    with open(args.path, "rb") as handle:
        data = handle.read()

    parts = split(data)
    print(f"{args.path}: {len(data)} 字节 -> {len(parts)} 段")
    for index, (blob, kind) in enumerate(parts):
        print(f"  [{index}] {kind}  {len(blob)} 字节")
        if args.out_dir:
            os.makedirs(args.out_dir, exist_ok=True)
            stem = os.path.splitext(os.path.basename(args.path))[0]
            out = os.path.join(args.out_dir, f"{stem}.{index:03d}.{kind}")
            with open(out, "wb") as handle:
                handle.write(blob)
    return 0


if __name__ == "__main__":
    sys.exit(main())
