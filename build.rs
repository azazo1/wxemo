// 复制到项目 build.rs, 通常不需要改动.
//
// 只在设置了 PROJECT_BUILD_VERSION 时注入构建版本号, 日常开发构建回落到 dev-build.
// 这里刻意不读取 .git: 让构建结果不随 commit 变化, 否则每次 commit 都会让增量编译
// 缓存失效, target/ 会持续膨胀.

fn main() {
    println!("cargo:rerun-if-env-changed=PROJECT_BUILD_VERSION");

    let version =
        std::env::var("PROJECT_BUILD_VERSION").unwrap_or_else(|_| "dev-build".to_string());
    println!("cargo:rustc-env=PROJECT_BUILD_VERSION={version}");
}
