#!/usr/bin/env python3

# 构建脚本共用的 UTF-8 输出设置.
import sys


def configure_output_encoding() -> None:
    """把 stdout 与 stderr 切到 UTF-8, 不支持 reconfigure 时保持原样."""
    for stream in (sys.stdout, sys.stderr):
        try:
            stream.reconfigure(encoding="utf-8")
        except (AttributeError, ValueError, OSError):
            pass
