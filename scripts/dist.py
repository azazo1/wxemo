#!/usr/bin/env python3

# 生成当前平台的发布产物 (Rust), 复制到项目 scripts/dist.py.
# 需要同时复制 scripts/archive.py, scripts/build-version.py, scripts/release_common.py 与 build.rs.

import json
import os
import shutil

import release_common as rc

# 只改下面几个常量:
PROJECT_NAME = "wxemo"
# 读包版本用的 crate 名, 与产物名不同时改这里.
PACKAGE_NAME = "wxemo"
BINARY_NAME = "wxemo"
# 冒烟检查用的参数, 要求二进制能报出版本号.
SMOKE_ARGS = ["--version"]
# 需要交叉编译时设置, 例如 aarch64-unknown-linux-musl;
# 交叉编译的产物无法本机执行, 只能做文件格式检查, 并用 TARGET_PLATFORM / TARGET_ARCH 命名.
RUST_TARGET = os.environ.get("RUST_TARGET", "")


def package_version_fallback():
    """仓库还没有版本 tag 时, 用 cargo metadata 里的包版本兜底; 读不到就返回 None."""
    metadata = rc.try_run(["cargo", "metadata", "--locked", "--no-deps", "--format-version", "1"])
    if not metadata:
        return None
    try:
        for package in json.loads(metadata)["packages"]:
            if package.get("name") == PACKAGE_NAME:
                return package.get("version")
    except (KeyError, ValueError):
        return None
    return None


def main():
    rc.configure_output_encoding()
    rc.repo_root()

    target_platform = rc.target_platform()
    target_arch = rc.target_arch()
    display_version, archive_version = rc.resolve_versions(
        rc.script_path("build-version.py"), package_version_fallback
    )
    print(f"构建 {PROJECT_NAME} {display_version} ({target_platform}-{target_arch})")

    # build.rs 只在设置了 PROJECT_BUILD_VERSION 时注入版本, 所以这里显式传给 cargo,
    # 否则直接跑 scripts/dist.py 时产物里还是 dev-build.
    cargo_env = os.environ.copy()
    cargo_env["PROJECT_BUILD_VERSION"] = display_version
    cargo_args = ["cargo", "build", "--release", "--locked"]
    if RUST_TARGET:
        cargo_args += ["--target", RUST_TARGET]
    rc.run(cargo_args, env=cargo_env, capture=False)

    binary = BINARY_NAME + (".exe" if target_platform == "windows" else "")
    build_dir = f"target/{RUST_TARGET}/release" if RUST_TARGET else "target/release"
    staging = rc.stage_dir(".tmp/dist-stage")
    binary_path = os.path.join(staging, binary)
    shutil.copy(os.path.join(build_dir, binary), binary_path)

    if RUST_TARGET:
        print("交叉编译产物无法本机执行, 只检查文件格式")
        rc.check_binary_format(binary_path, target_platform)
    else:
        rc.smoke_check([binary_path], SMOKE_ARGS, display_version)

    rc.archive(
        staging,
        [binary],
        PROJECT_NAME,
        archive_version,
        target=(target_platform, target_arch),
    )


if __name__ == "__main__":
    main()
