[private]
default:
    @just --list

# 编译当前平台的优化 CLI, 使用 dev-build 版本.
build:
    cargo build --release --locked

# 测试加解密, 搜索和容器字节守恒.
test:
    cargo test --locked

# 运行严格 Clippy 检查.
lint:
    cargo clippy --locked --all-targets -- -D warnings

# 检查 Rust 代码格式, 不修改源码.
fmt-check:
    cargo fmt --check

# just run --help
# 从源码运行 CLI.
run *args:
    cargo run --locked -- {{args}}

# 生成当前平台的归档并自动注入构建版本.
dist:
    uv run --no-project python scripts/dist.py
