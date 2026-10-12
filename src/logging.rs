//! 日志初始化. 进度条与 tracing 输出共用 stderr, 用 tracing-indicatif 协调.

use tracing_indicatif::IndicatifLayer;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::EnvFilter;

/// 环境变量名, 可覆盖日志级别.
const ENV: &str = "WXEMO_LOG";

/// 初始化全局日志. `verbose` 为真时默认级别降到 debug.
pub fn init(verbose: bool) {
    let default = if verbose { "debug" } else { "info" };
    let filter = EnvFilter::try_from_env(ENV).unwrap_or_else(|_| EnvFilter::new(default));

    let indicatif_layer = IndicatifLayer::new();
    // 先取 stderr writer, 再把 layer 交给 registry, 否则 layer 已被移动.
    let writer = indicatif_layer.get_stderr_writer();
    tracing_subscriber::registry()
        .with(indicatif_layer)
        .with(
            tracing_subscriber::fmt::layer()
                .with_target(false)
                .with_writer(writer),
        )
        .with(filter)
        .init();
}
