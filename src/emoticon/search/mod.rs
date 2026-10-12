//! seed 搜索: 公共类型, 后端分发与命中复核.
//!
//! 判定条件: 全部样本用候选密钥解出的首块都命中已知图片魔数.
//!
//! 之所以不依赖 PKCS#7 填充做判定: 微信是否加填充不便先验假定, 若它不加填充,
//! 用填充做必要条件会直接漏掉正确候选. 而要求全部样本首块命中魔数本身已经足够强,
//! 3 个样本时最弱的 3 字节 JPEG 魔数也把误报期望压到 1e-5 以下.
//!
//! 填充情况会记录在日志与结果里, 作为诊断信息而不是判定依据.
//!
//! 后端有两种: CPU 多核 (`scalar`) 与 CUDA (`cuda`, 需要 `cuda` feature).
//! 两者共用这里的 `Sample` / `SearchSpec` / `Hit` 与命中复核 `confirm`.

pub(crate) mod scalar;

#[cfg(feature = "cuda")]
pub(crate) mod cuda;

#[cfg(test)]
pub(crate) mod testing;

use std::fmt;
use std::str::FromStr;

use anyhow::{bail, Result};
use indicatif::ProgressBar;
use tracing::{info, warn};

use crate::crypto::{
    decrypt_all, decrypt_first_block, derive_key, match_magic, new_cipher, valid_padding,
    MAX_MESSAGE,
};

/// 判定接受的最小魔数长度. 2 字节的魔数太弱, 不作为依据.
pub(crate) const MIN_STRENGTH: usize = 3;

/// 一个样本文件.
pub struct Sample {
    /// 用于展示的名字.
    pub name: String,
    /// 密文首块, 用于魔数判定.
    pub prefix: Vec<u8>,
    /// 完整密文, 用于命中后的复核.
    pub body: Vec<u8>,
}

/// 一次搜索的全部输入.
pub struct SearchSpec {
    /// 搜索起点 (含).
    pub start: u64,
    /// 搜索终点 (不含).
    pub end: u64,
    /// 账号标识候选, 每个都会被尝试.
    pub wxids: Vec<Vec<u8>>,
    /// 样本文件.
    pub samples: Vec<Sample>,
}

/// 命中的候选.
#[derive(Debug, Clone)]
pub struct Hit {
    /// 账号级种子.
    pub seed: u64,
    /// 命中的账号标识.
    pub wxid: Vec<u8>,
    /// 派生出的密钥.
    pub key: [u8; 16],
    /// 命中的图片格式.
    pub format: String,
    /// 命中的样本名与对应格式. 由调用方展示, 这里保留.
    pub matched: Vec<String>,
    /// 命中时全部样本的填充是否都合法.
    pub padding_ok: bool,
}

/// 用户在命令行上请求的后端.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Backend {
    /// 能用 CUDA 就用 CUDA, 否则回落 CPU.
    #[default]
    Auto,
    /// 只用 CPU.
    Cpu,
    /// 只用 CUDA, 不可用时明确报错.
    Cuda,
}

impl FromStr for Backend {
    type Err = String;

    fn from_str(text: &str) -> std::result::Result<Self, Self::Err> {
        match text {
            "auto" => Ok(Self::Auto),
            "cpu" => Ok(Self::Cpu),
            "cuda" => Ok(Self::Cuda),
            other => Err(format!("未知后端 {other}, 可选 auto / cpu / cuda")),
        }
    }
}

impl fmt::Display for Backend {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let text = match self {
            Self::Auto => "auto",
            Self::Cpu => "cpu",
            Self::Cuda => "cuda",
        };
        formatter.write_str(text)
    }
}

/// 已经确定下来的后端.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Resolved {
    /// CPU 多核.
    Cpu,
    /// CUDA.
    Cuda,
}

impl fmt::Display for Resolved {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Cpu => "cpu",
            Self::Cuda => "cuda",
        })
    }
}

/// 把用户请求解析成实际可用的后端.
///
/// `auto` 会尝试 CUDA, 失败时打日志回落 CPU. 显式 `cuda` 不可用时直接报错,
/// 不做静默回落, 否则用户会以为自己在跑 GPU.
pub fn resolve(requested: Backend) -> Result<Resolved> {
    match requested {
        Backend::Cpu => Ok(Resolved::Cpu),
        Backend::Cuda => match cuda_available() {
            Ok(()) => Ok(Resolved::Cuda),
            Err(reason) => bail!("指定了 --backend cuda 但 CUDA 不可用: {reason:#}"),
        },
        Backend::Auto => match cuda_available() {
            Ok(()) => {
                info!("检测到可用 CUDA 设备, 使用 cuda 后端");
                Ok(Resolved::Cuda)
            }
            Err(reason) => {
                warn!(reason = %format!("{reason:#}"), "CUDA 不可用, 回落到 cpu 后端");
                Ok(Resolved::Cpu)
            }
        },
    }
}

/// 检查 CUDA 是否可用.
#[cfg(feature = "cuda")]
fn cuda_available() -> Result<()> {
    cuda::probe().map(|info| {
        info!(
            device = %info.name,
            compute_capability = %format!("{}.{}", info.major, info.minor),
            "CUDA 设备就绪"
        );
    })
}

/// 检查 CUDA 是否可用.
#[cfg(not(feature = "cuda"))]
fn cuda_available() -> Result<()> {
    bail!("本二进制编译时没有开启 cuda feature, 请用 `cargo build --features cuda` 重新编译")
}

/// 按已解析的后端执行搜索, 找到即返回.
pub fn run(spec: &SearchSpec, bar: &ProgressBar, backend: Resolved) -> Result<Option<Hit>> {
    match backend {
        Resolved::Cpu => Ok(scalar::run(spec, bar)),
        #[cfg(feature = "cuda")]
        Resolved::Cuda => cuda::run(spec, bar),
        #[cfg(not(feature = "cuda"))]
        Resolved::Cuda => bail!("本二进制编译时没有开启 cuda feature"),
    }
}

/// 首块魔数是否达到判定强度.
pub(crate) fn strong_magic(block: &[u8; 16]) -> bool {
    match_magic(block).is_some_and(|(_, len)| len >= MIN_STRENGTH)
}

/// 复核一个候选: 全部样本首块都命中魔数才算命中, 命中后整文件解密确认填充情况.
///
/// CUDA 内核只负责把候选筛出来, 真正的判定与 `Hit` 的构造都回到这里,
/// 保证两个后端的判定语义完全一致.
pub(crate) fn confirm(spec: &SearchSpec, seed: u64, wxid_index: usize) -> Option<Hit> {
    let wxid = spec.wxids.get(wxid_index)?;
    let mut message = [0u8; MAX_MESSAGE];
    let key = derive_key(seed, wxid, &mut message);
    let cipher = new_cipher(&key);

    let mut best: Option<(&'static str, usize)> = None;
    let mut matched = Vec::new();
    for sample in &spec.samples {
        let plain = decrypt_first_block(&cipher, &key, &sample.prefix);
        match match_magic(&plain) {
            Some((name, len)) if len >= MIN_STRENGTH => {
                if best.is_none_or(|(_, best_len)| len > best_len) {
                    best = Some((name, len));
                }
                matched.push(format!("{}:{}", sample.name, name));
            }
            _ => return None,
        }
    }
    let (format, _) = best?;

    // 命中后整文件复核一次, 顺便记录填充情况.
    let padding_ok = spec.samples.iter().all(|sample| {
        let mut plain = vec![0u8; sample.body.len()];
        decrypt_all(&cipher, &key, &sample.body, &mut plain);
        valid_padding(&plain)
    });

    info!(
        seed,
        wxid = %String::from_utf8_lossy(wxid),
        format,
        padding_ok,
        matched = ?matched,
        "命中候选"
    );
    Some(Hit {
        seed,
        wxid: wxid.clone(),
        key,
        format: format.to_string(),
        matched,
        padding_ok,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backend_parses_and_formats() {
        assert_eq!("auto".parse::<Backend>().unwrap(), Backend::Auto);
        assert_eq!("cpu".parse::<Backend>().unwrap(), Backend::Cpu);
        assert_eq!("cuda".parse::<Backend>().unwrap(), Backend::Cuda);
        assert!("gpu".parse::<Backend>().is_err());
        assert_eq!(Backend::default(), Backend::Auto);
        assert_eq!(Backend::Cuda.to_string(), "cuda");
        assert_eq!(Resolved::Cpu.to_string(), "cpu");
    }

    #[test]
    fn explicit_cpu_backend_always_resolves_to_cpu() {
        assert_eq!(resolve(Backend::Cpu).unwrap(), Resolved::Cpu);
    }

    /// 没有 cuda feature 时 auto 一定回落 CPU.
    #[cfg(not(feature = "cuda"))]
    #[test]
    fn auto_falls_back_to_cpu_without_cuda_feature() {
        assert_eq!(resolve(Backend::Auto).unwrap(), Resolved::Cpu);
    }

    #[cfg(not(feature = "cuda"))]
    #[test]
    fn explicit_cuda_without_feature_errors() {
        let error = resolve(Backend::Cuda).unwrap_err().to_string();
        assert!(error.contains("cuda feature"), "{error}");
    }
}
