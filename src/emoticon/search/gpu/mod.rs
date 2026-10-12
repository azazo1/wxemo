//! wgpu 计算后端.
//!
//! 搜索语义与 CPU 实现完全一致: 候选必须让全部样本的密文首块解出已知图片魔数,
//! 命中后再用整文件解密复核填充. 区别只是判定在 GPU 上并行完成.
//!
//! 流程: 把 `[start, end)` 里的候选按批提交给计算管线, 每批读回命中的候选序号,
//! 交给与 CPU 侧同一套复核逻辑确认. 一旦确认命中就停止提交后续批次, 这就是提前退出.
//! 批次之间不阻塞等待, 读回用 `map_async` 加轮询推进进度条.

pub mod host;

use std::time::Duration;

use anyhow::{anyhow, bail, Result};
use indicatif::ProgressBar;
use tracing::{debug, info, warn};

use crate::emoticon::search::scalar;
use crate::emoticon::search::{Hit, SearchSpec};

use host::{Kernel, MAX_HITS};

/// GPU 能处理的 seed 上界: seed 在着色器里是 u32.
pub const SEED_LIMIT: u64 = 1 << 32;

/// 每完成这么多批输出一次进度日志.
const LOG_EVERY: u64 = 64;

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
    while seed < spec.end {
        let span = (spec.end - seed).min(batch_seeds as u64) as u32;
        let base = seed as u32;
        let work_count = span * wxid_count;

        let hits = kernel.dispatch(base, work_count)?;
        let done = span as u64;
        bar.inc(done);
        seed += done;
        batches += 1;
        if batches.is_multiple_of(LOG_EVERY) {
            info!(seed, end = spec.end, "GPU 搜索进度");
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
            if let Some(hit) = scalar::verify_candidate(spec, candidate, wxid) {
                bar.set_position(spec.end - spec.start);
                return Ok(Some(hit));
            }
            warn!(candidate, "GPU 报出的候选没有通过复核, 继续搜索");
        }
    }

    bar.set_position(spec.end - spec.start);
    Ok(None)
}

/// 把错误转成便于日志展示的形式.
pub fn describe(error: &anyhow::Error) -> String {
    format!("{error:#}")
}

/// 便于调用方构造统一错误.
pub fn unavailable(error: &anyhow::Error) -> anyhow::Error {
    anyhow!("wgpu 后端不可用: {}", describe(error))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::emoticon::crypto::{derive_key, MAX_MESSAGE};
    use crate::emoticon::search::scalar::test_support::format_samples;

    /// 用合成样本让 GPU 后端在窄范围里真的找到已知 seed, 并断言派生出的密钥正确.
    ///
    /// 没有可用 adapter 时跳过; 有 adapter 却起不来或算错, 就必须失败.
    #[test]
    fn wgpu_finds_known_seed_and_derives_the_same_key() {
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
            samples: format_samples(&key),
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
            samples: format_samples(&key),
        };
        let bar = ProgressBar::hidden();
        assert!(run(&spec, &bar).expect("wgpu 后端应当可用").is_none());
    }
}
