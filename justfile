# 微信 4.x 自定义表情包加解密工具.
#
# 变量说明:
#   EMOTICON_DIR  账号的 business/emoticon 目录
#   OUT_DIR       还原出的图片输出目录

EMOTICON_DIR := "emoticon"
OUT_DIR := "out/emoticons"

[private]
default:
    @just --list

# just build
# 编译 seed 搜索器 (release).
build:
    cargo build --release --manifest-path seedsearch/Cargo.toml

# just test
# 运行搜索器的单元测试.
test:
    cargo test --release --manifest-path seedsearch/Cargo.toml

# just selfcheck
# 端到端自验证: 造已知 seed 的样本, 再让搜索器找回来.
selfcheck: build
    python3 scripts/selfcheck.py seedsearch/target/release/seedsearch .tmp/selfcheck

# just search <密文目录> <账号标识>
# 例如: just search ~/xwechat_files/wxid_abc_7e1e/business/emoticon wxid_abc
# 暴力搜索当前账号的 seed, 结果写入 seed-result.txt.
search emoticon_dir wxid:
    #!/usr/bin/env bash
    set -euo pipefail
    samples=()
    while IFS= read -r f; do samples+=("$f"); done < <(find "{{emoticon_dir}}/Persist" -type f | head -3)
    if [ "${#samples[@]}" -lt 3 ]; then
        echo "样本不足, 需要至少 3 个文件" >&2
        exit 1
    fi
    args=()
    for f in "${samples[@]}"; do args+=(--sample "$f"); done
    seedsearch/target/release/seedsearch \
        "${args[@]}" \
        --wxid "{{wxid}}" \
        --start 0 --end 4294967296 \
        --out seed-result.txt

# just extract <密文目录> <输出目录> <key>
# 用求出的 key 还原全部表情包.
extract emoticon_dir out_dir key:
    python3 scripts/extract_emoticons.py "{{emoticon_dir}}" "{{out_dir}}" --key "{{key}}"

# just split <文件>
# 查看一个文件会被切成几段 (只打印, 不写文件).
split path:
    python3 scripts/split_container.py "{{path}}"
