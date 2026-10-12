//! seed 搜索: 公共类型, 后端选择与调度.
//!
//! 判定条件: 全部样本用候选密钥解出的首块都命中已知图片魔数.
//!
//! 之所以不依赖 PKCS#7 填充做判定: 微信是否加填充不便先验假定, 若它不加填充,
//! 用填充做必要条件会直接漏掉正确候选. 而要求全部样本首块命中魔数本身已经足够强,
//! 3 个样本时最弱的 3 字节 JPEG 魔数也把误报期望压到 1e-5 以下.
//!
//! 填充情况会记录在日志与结果里, 作为诊断信息而不是判定依据.
//!
//! 后端划分:
//!
//! - [`scalar`] 是多核 CPU 实现, 也是语义基准.
//! - [`gpu`] 用 wgpu 把同样的判定放到 GPU 上, 一套 WGSL 覆盖 Metal / Vulkan / DX12.
//!
//! 两个后端共用 [`scalar::verify_candidate`] 做命中复核, 因此判定条件不会漂移.

pub mod gpu;
pub mod scalar;

use std::str::FromStr;

use anyhow::{bail, Result};
use indicatif::ProgressBar;
use tracing::{info, warn};

pub use scalar::Hit;
pub use scalar::{run as run_scalar, Sample, SearchSpec};

/// 搜索后端.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Backend {
    /// 能用 wgpu 就用, 否则回落到 CPU.
    #[default]
    Auto,
    /// 纯 CPU 多核实现.
    Cpu,
    /// wgpu 计算后端, 不可用时直接报错.
    Wgpu,
}

impl Backend {
    /// 日志里展示用的名字.
    pub fn name(self) -> &'static str {
        match self {
            Backend::Auto => "auto",
            Backend::Cpu => "cpu",
            Backend::Wgpu => "wgpu",
        }
    }
}

impl FromStr for Backend {
    type Err = anyhow::Error;

    fn from_str(value: &str) -> Result<Self> {
        match value.to_ascii_lowercase().as_str() {
            "auto" => Ok(Backend::Auto),
            "cpu" | "scalar" => Ok(Backend::Cpu),
            "wgpu" | "gpu" => Ok(Backend::Wgpu),
            other => bail!("未知的 backend: {other}, 可选 auto / cpu / wgpu"),
        }
    }
}

impl std::fmt::Display for Backend {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.name())
    }
}

/// 按选定的后端搜索, 找到即返回.
///
/// `auto` 先试 wgpu, 后端起不来就记日志回落到 CPU.
/// 显式指定 `wgpu` 时后端不可用会返回错误, 不做静默回落.
pub fn run(spec: &SearchSpec, bar: &ProgressBar, backend: Backend) -> Result<Option<Hit>> {
    match backend {
        Backend::Cpu => {
            info!(backend = "cpu", "使用 CPU 后端");
            Ok(run_scalar(spec, bar))
        }
        Backend::Wgpu => {
            info!(backend = "wgpu", "使用 GPU 后端, 不可用时会直接报错");
            run_wgpu(spec, bar).map_err(|error| gpu::unavailable(&error))
        }
        Backend::Auto => match run_wgpu(spec, bar) {
            Ok(hit) => Ok(hit),
            Err(error) => {
                // 初始化失败时进度条还停在起点; 中途失败则要从头来过, 所以先归零.
                info!(
                    backend = "cpu",
                    reason = %gpu::describe(&error),
                    "auto 回落到 CPU 后端"
                );
                bar.set_position(0);
                Ok(run_scalar(spec, bar))
            }
        },
    }
}

/// 在 GPU 能覆盖的部分用 GPU, 剩下的交给 CPU, 保证语义与纯 CPU 一致.
fn run_wgpu(spec: &SearchSpec, bar: &ProgressBar) -> Result<Option<Hit>> {
    if spec.start >= spec.end {
        return Ok(None);
    }

    // seed 在着色器里是 u32, 超出 2^32 的部分只能交给 CPU.
    let gpu_end = spec.end.min(gpu::SEED_LIMIT);
    if spec.start < gpu_end {
        let gpu_spec = SearchSpec {
            start: spec.start,
            end: gpu_end,
            wxids: spec.wxids.clone(),
            samples: spec.samples.clone(),
        };
        if let Some(hit) = gpu::run(&gpu_spec, bar)? {
            return Ok(Some(hit));
        }
    }

    if spec.end > gpu::SEED_LIMIT {
        let cpu_start = spec.start.max(gpu::SEED_LIMIT);
        warn!(
            start = cpu_start,
            end = spec.end,
            "范围超出 seed 的 32 位表示, 这部分回落到 CPU"
        );
        // 进度条已经推进过 GPU 覆盖的部分, 这里用一个隐藏进度条再补记.
        let rest = SearchSpec {
            start: cpu_start,
            end: spec.end,
            wxids: spec.wxids.clone(),
            samples: spec.samples.clone(),
        };
        let hidden = ProgressBar::hidden();
        let hit = run_scalar(&rest, &hidden);
        bar.inc(rest.end - rest.start);
        return Ok(hit);
    }

    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backend_parsing_accepts_supported_names() {
        assert_eq!(Backend::from_str("auto").unwrap(), Backend::Auto);
        assert_eq!(Backend::from_str("CPU").unwrap(), Backend::Cpu);
        assert_eq!(Backend::from_str("wgpu").unwrap(), Backend::Wgpu);
        assert!(Backend::from_str("cuda").is_err());
    }

    #[test]
    fn cpu_backend_agrees_with_direct_scalar_run() {
        use crate::emoticon::crypto::{derive_key, MAX_MESSAGE};

        let seed = 87654321u64;
        let wxid = b"wxid_example123456";
        let mut message = [0u8; MAX_MESSAGE];
        let key = derive_key(seed, wxid, &mut message);
        let samples = vec![crate::emoticon::search::scalar::test_support::sample(
            "a.png",
            crate::emoticon::search::scalar::test_support::encrypt(
                &key,
                b"\x89PNG\r\n\x1a\n0123456789abcdef",
            ),
        )];
        let spec = SearchSpec {
            start: seed - 2,
            end: seed + 2,
            wxids: vec![wxid.to_vec()],
            samples,
        };

        let bar = ProgressBar::hidden();
        let via_dispatch = run(&spec, &bar, Backend::Cpu).unwrap().expect("应当命中");
        let bar = ProgressBar::hidden();
        let direct = run_scalar(&spec, &bar).expect("应当命中");
        assert_eq!(via_dispatch.seed, direct.seed);
        assert_eq!(via_dispatch.key, direct.key);
        assert_eq!(via_dispatch.key, key);
    }
}
