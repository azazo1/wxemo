//! wgpu 计算后端.
//!
//! 搜索语义与 CPU 实现完全一致: 候选必须让全部样本的密文首块解出已知图片魔数,
//! 命中后再用整文件解密复核填充. 区别只是判定在 GPU 上并行完成.
//!
//! 流程: 把 `[start, end)` 里的候选按批提交给计算管线, 每批读回命中的候选序号,
//! 交给与 CPU 侧同一套复核逻辑确认. 一旦确认命中就停止提交后续批次, 这就是提前退出.
//! 批次之间不阻塞等待, 读回用 `map_async` 加轮询推进进度条.

pub mod host;

use std::time::{Duration, Instant};

use anyhow::{bail, Result};
use indicatif::ProgressBar;
use tracing::{debug, info, warn};

use crate::emoticon::search::{confirm, Hit, SearchSpec};

use host::{Kernel, MAX_HITS};

/// GPU 能处理的 seed 上界: seed 在着色器里是 u32.
pub const SEED_LIMIT: u64 = 1 << 32;

/// 进度日志的最小间隔.
///
/// 不能按批次计数: 一批能覆盖几百万个候选, 批次推进很快, 固定批数间隔会变成
/// 每秒刷一条. 按时间节流才对范围大小不敏感, 也仍能让人看出进程是否卡住.
const PROGRESS_LOG_INTERVAL: Duration = Duration::from_secs(30);

/// 在 `[spec.start, spec.end)` 上跑 GPU 搜索, 范围必须已经裁剪到 u32 之内.
///
/// 返回 `Ok(None)` 表示范围内没有命中. 返回 `Err` 表示后端不可用, 由调用方决定回落策略.
pub fn run(spec: &SearchSpec, bar: &ProgressBar) -> Result<Option<Hit>> {
    if spec.end > SEED_LIMIT {
        bail!("GPU 后端的 seed 上界是 {SEED_LIMIT}, 收到 {}", spec.end);
    }
    if spec.start >= spec.end {
        return Ok(None);
    }

    let kernel = Kernel::new(spec)?;
    info!(
        adapter = kernel.adapter_name(),
        backend = kernel.backend_name(),
        device_type = kernel.device_type(),
        "使用 wgpu 后端"
    );

    let wxid_count = spec.wxids.len() as u32;
    let batch_capacity = kernel.batch_capacity();
    // 候选数必须是账号标识数的整数倍, 否则一批里会切到半个 seed.
    let batch_seeds = (batch_capacity / wxid_count).max(1);
    debug!(
        batch_seeds,
        wxid_count, "GPU 批次划分完成, 每批最多这么多 seed"
    );

    // GPU 路径自己驱动进度条, 让 spinner 在长时间计算时也能转.
    bar.enable_steady_tick(Duration::from_millis(100));

    let mut seed = spec.start;
    let mut batches = 0u64;
    let mut last_log = Instant::now();
    while seed < spec.end {
        let span = (spec.end - seed).min(batch_seeds as u64) as u32;
        let base = seed as u32;
        let work_count = span * wxid_count;

        let hits = kernel.dispatch(base, work_count)?;
        let done = span as u64;
        bar.inc(done);
        seed += done;
        batches += 1;
        if last_log.elapsed() >= PROGRESS_LOG_INTERVAL {
            last_log = Instant::now();
            info!(seed, end = spec.end, batches, "GPU 搜索进度");
        }

        if hits.is_empty() {
            continue;
        }
        if hits.len() >= MAX_HITS {
            warn!(
                slots = hits.len(),
                base, "一批里的命中槽位已满, 结果可能不完整"
            );
        }

        // 按候选序号升序复核, 第一个通过的就是结果.
        for work in hits {
            let offset = work / wxid_count;
            let wxid_index = (work % wxid_count) as usize;
            let candidate = base as u64 + offset as u64;
            if candidate >= spec.end {
                continue;
            }
            let wxid = &spec.wxids[wxid_index];
            debug!(candidate, wxid = %String::from_utf8_lossy(wxid), "GPU 报出候选, 开始复核");
            if let Some(hit) = confirm(spec, candidate, wxid_index) {
                bar.set_position(spec.end - spec.start);
                return Ok(Some(hit));
            }
            warn!(candidate, "GPU 报出的候选没有通过复核, 继续搜索");
        }
    }

    bar.set_position(spec.end - spec.start);
    Ok(None)
}

/// wgpu 后端是否可用: 至少要有一个可枚举的 adapter.
pub fn available() -> bool {
    host::adapter_count() > 0
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::emoticon::crypto::{derive_key, MAX_MESSAGE};
    use crate::emoticon::search::testing::three_samples;
    use std::sync::Mutex;

    /// 串行化 GPU 测试.
    ///
    /// `run` 每次调用都会各建一套 wgpu 设备与管线, 而 cargo test 默认并行跑用例.
    /// 两个设备同时创建销毁, 在没有独显的机器上会直接在图形驱动里崩掉整个测试进程
    /// (CI 的 Windows ARM runner 就是这样拿到 STATUS_ACCESS_VIOLATION 的),
    /// 崩掉时后面用例的结果一个都打印不出来, 表现为难以定位的随机失败.
    static GPU_TEST_LOCK: Mutex<()> = Mutex::new(());

    /// 取到锁再往下跑; 前一个用例 panic 过也要继续, 否则后续用例会被连带毒化.
    fn serialize() -> std::sync::MutexGuard<'static, ()> {
        GPU_TEST_LOCK
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
    }

    /// 用合成样本让 GPU 后端在窄范围里真的找到已知 seed, 并断言派生出的密钥正确.
    ///
    /// 没有可用 adapter 时跳过; 有 adapter 却起不来或算错, 就必须失败.
    #[test]
    fn wgpu_finds_known_seed_and_derives_the_same_key() {
        let _guard = serialize();

        let seed = 123456789u64;
        let wxid = b"wxid_benchmark123456";
        let mut message = [0u8; MAX_MESSAGE];
        let key = derive_key(seed, wxid, &mut message);

        if host::adapter_count() == 0 {
            eprintln!("跳过 wgpu 正确性测试: 当前环境没有可用的 wgpu adapter");
            return;
        }

        let spec = SearchSpec {
            start: seed - 40,
            end: seed + 40,
            wxids: vec![wxid.to_vec()],
            samples: three_samples(&key),
        };
        let bar = ProgressBar::hidden();
        let hit = run(&spec, &bar)
            .expect("有 adapter 时 wgpu 后端必须可用")
            .expect("应当在窄范围内命中已知 seed");

        assert_eq!(hit.seed, seed);
        assert_eq!(hit.wxid, wxid.to_vec());
        assert_eq!(hit.key, key, "GPU 派生的密钥必须与 CPU 一致");
        assert_eq!(
            crate::config::hex(&hit.key),
            "ad2b95adcc7e968bae8ba189adaf5fc2"
        );
        assert!(hit.padding_ok);
    }

    /// 范围里不含正确 seed 时, GPU 后端必须返回未命中而不是乱报.
    #[test]
    fn wgpu_reports_nothing_for_unrelated_range() {
        let _guard = serialize();

        let seed = 424242u64;
        let wxid = b"wxid_benchmark123456";
        let mut message = [0u8; MAX_MESSAGE];
        let key = derive_key(seed, wxid, &mut message);

        if host::adapter_count() == 0 {
            eprintln!("跳过 wgpu 空范围测试: 当前环境没有可用的 wgpu adapter");
            return;
        }

        let spec = SearchSpec {
            start: 900000,
            end: 900120,
            wxids: vec![wxid.to_vec()],
            samples: three_samples(&key),
        };
        let bar = ProgressBar::hidden();
        assert!(run(&spec, &bar).expect("wgpu 后端应当可用").is_none());
    }
}
