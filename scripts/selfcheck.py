"""端到端自验证: 用已知 seed 构造密文样本, 再交给 seedsearch 找回.

用法: python3 selfcheck.py <seedsearch 可执行文件> <输出目录>
"""

from __future__ import annotations

import hashlib
import os
import subprocess
import sys

from Crypto.Cipher import AES

PNG_MAGIC = b"\x89PNG\r\n\x1a\n"
GIF_MAGIC = b"GIF89a"
JPEG_MAGIC = b"\xff\xd8\xff\xe0"


def derive(seed: int, wxid: str) -> bytes:
    return hashlib.md5(f"{seed}{wxid}EMOTICON".encode()).digest()[:16]


def encrypt(key: bytes, plain: bytes, pad: bool) -> bytes:
    if pad:
        n = 16 - len(plain) % 16
        plain = plain + bytes([n]) * n
    cipher = AES.new(key, AES.MODE_CBC, key)
    return cipher.encrypt(plain)


def make_png(size: int) -> bytes:
    body = bytearray()
    body += b"\x00\x00\x00\rIHDR"
    body += (64).to_bytes(4, "big") + (64).to_bytes(4, "big")
    body += bytes([8, 6, 0, 0, 0])
    body += os.urandom(size)
    return PNG_MAGIC + bytes(body)


def make_gif(size: int) -> bytes:
    return GIF_MAGIC + b"\x40\x40\x80\x00\x00" + os.urandom(size)


def make_jpeg(size: int) -> bytes:
    return JPEG_MAGIC + b"\x00\x10JFIF\x00" + os.urandom(size)


def main() -> int:
    exe, outdir = sys.argv[1], sys.argv[2]
    os.makedirs(outdir, exist_ok=True)

    seed = 87654321
    wxid = "wxid_example123456"
    key = derive(seed, wxid)
    print(f"目标 seed = {seed}, wxid = {wxid}, key = {key.hex()}")

    # 用三个不同格式的样本, 都补齐到 16 的整数倍
    plans = [("a.png", make_png(400), True), ("b.gif", make_gif(600), True), ("c.jpg", make_jpeg(500), False)]
    paths = []
    for name, plain, pad in plans:
        # 需要保证明文长度对齐, 不补齐时手动截到 16 的倍数
        if not pad:
            plain = plain[: len(plain) - len(plain) % 16]
        data = encrypt(key, plain, pad)
        path = os.path.join(outdir, name)
        with open(path, "wb") as handle:
            handle.write(data)
        paths.append(path)
        print(f"写入 {path} ({len(data)} 字节)")

    cmd = [exe, "--start", str(seed - 5), "--end", str(seed + 5), "--wxid", wxid,
           "--out", os.path.join(outdir, "result.txt")]
    for path in paths:
        cmd += ["--sample", path]
    print("执行:", " ".join(cmd))
    proc = subprocess.run(cmd, capture_output=True, text=True)
    print("--- stdout ---")
    print(proc.stdout)
    print("--- stderr (尾部) ---")
    print("\n".join(proc.stderr.strip().splitlines()[-6:]))
    if f"seed  = {seed}" in proc.stdout:
        print("*** 自验证通过 ***")
        return 0
    print("*** 自验证失败 ***")
    return 1


if __name__ == "__main__":
    sys.exit(main())
