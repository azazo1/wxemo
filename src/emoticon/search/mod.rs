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
//! 两种后端的判定条件完全一致, 区别只在并行粒度:
//!
//! - `scalar`: 每个候选独立做标量 MD5 与 AES, 由 rayon 在多个核心上并行.
//! - `simd`: 一批候选共用一次向量化 MD5, 每路各自做 AES 首块判定.
//!
//! 选路由 [`Backend::resolve`] 决定, 显式指定 `simd` 而本机不支持时直接报错,
//! 不会静默退回标量路径.

mod scalar;
mod simd;

#[cfg(test)]
pub(crate) mod testing;

use std::fmt;
use std::str::FromStr;

use anyhow::{bail, Result};
use indicatif::ProgressBar;
use tracing::info;

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

/// 用户选择的搜索后端.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Backend {
    /// 本机支持 SIMD 就用 SIMD, 否则用标量.
    #[default]
    Auto,
    /// 标量路径, 只靠多核并行.
    Cpu,
    /// 多路向量化路径.
    Simd,
}

impl FromStr for Backend {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "auto" => Ok(Backend::Auto),
            "cpu" => Ok(Backend::Cpu),
            "simd" => Ok(Backend::Simd),
            other => Err(format!("未知后端 {other}, 可选 auto / cpu / simd")),
        }
    }
}

impl fmt::Display for Backend {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let name = match self {
            Backend::Auto => "auto",
            Backend::Cpu => "cpu",
            Backend::Simd => "simd",
        };
        formatter.write_str(name)
    }
}

impl Backend {
    /// 结合本机能力确定实际使用的后端.
    ///
    /// `auto` 会按能力自动选择, 显式指定 `simd` 而能力不足时返回错误,
    /// 避免用户以为在用 SIMD 而实际跑的是标量.
    pub fn resolve(self) -> Result<Resolved> {
        let capable = simd::available();
        match self {
            Backend::Cpu => Ok(Resolved::Cpu),
            Backend::Simd if capable => Ok(Resolved::Simd),
            Backend::Simd => bail!(
                "本机不支持 simd 后端, 需要 {}; 可改用 --backend cpu",
                simd::REQUIREMENT
            ),
            Backend::Auto if capable => Ok(Resolved::Simd),
            Backend::Auto => Ok(Resolved::Cpu),
        }
    }
}

/// 实际执行搜索的后端.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Resolved {
    /// 标量多核.
    Cpu,
    /// SIMD 多路.
    Simd,
}

impl fmt::Display for Resolved {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let name = match self {
            Resolved::Cpu => "cpu",
            Resolved::Simd => "simd",
        };
        formatter.write_str(name)
    }
}

impl Resolved {
    /// 该后端一次并行处理的候选数. 标量路径为 1.
    pub fn lanes(self) -> usize {
        match self {
            Resolved::Cpu => 1,
            Resolved::Simd => simd::LANES,
        }
    }
}

/// 在 `spec` 指定的范围内并行搜索, 找到即返回.
pub fn run(spec: &SearchSpec, bar: &ProgressBar, backend: Resolved) -> Option<Hit> {
    info!(backend = %backend, lanes = backend.lanes(), "搜索后端");
    match backend {
        Resolved::Cpu => scalar::run(spec, bar),
        Resolved::Simd => simd::run(spec, bar),
    }
}

#[cfg(test)]
mod tests {
    use super::testing::{encrypt, key_for, sample};
    use super::*;

    /// SIMD 后端应当和标量后端一样找到已知 seed.
    #[test]
    fn simd_backend_finds_known_seed() {
        if !simd::available() {
            return;
        }
        let seed = 87654321u64;
        let wxid = b"wxid_example123456";
        let key = key_for(seed, wxid);

        let spec = SearchSpec {
            start: seed - 3,
            end: seed + 3,
            wxids: vec![wxid.to_vec()],
            samples: vec![
                sample("a.png", encrypt(&key, b"\x89PNG\r\n\x1a\n0123456789abcdef")),
                sample("b.gif", encrypt(&key, b"GIF89a0123456789abcdef")),
                sample("c.jpg", encrypt(&key, b"\xff\xd8\xff\xe00123456789abcdef")),
            ],
        };

        let bar = ProgressBar::hidden();
        let hit = simd::run(&spec, &bar).expect("SIMD 后端应当找到 seed");
        assert_eq!(hit.seed, seed);
        assert_eq!(hit.wxid, wxid.to_vec());
        assert_eq!(hit.key, key);
        assert!(hit.padding_ok);
        assert_eq!(hit.format, "png");
    }

    /// 批次边界和十进制位数切换处都不能漏候选.
    #[test]
    fn simd_backend_crosses_digit_boundary() {
        if !simd::available() {
            return;
        }
        let seed = 100000u64;
        let wxid = b"wxid_example123456";
        let key = key_for(seed, wxid);

        let spec = SearchSpec {
            start: 99995,
            end: 100005,
            wxids: vec![wxid.to_vec()],
            samples: vec![sample(
                "a.png",
                encrypt(&key, b"\x89PNG\r\n\x1a\n0123456789abcdef"),
            )],
        };

        let bar = ProgressBar::hidden();
        let hit = simd::run(&spec, &bar).expect("跨位数边界也应当找到 seed");
        assert_eq!(hit.seed, seed);
        assert_eq!(hit.key, key);
    }

    /// 范围里没有正确 seed 时不能误报.
    #[test]
    fn simd_backend_reports_nothing_when_out_of_range() {
        if !simd::available() {
            return;
        }
        let seed = 4242u64;
        let wxid = b"wxid_example123456";
        let key = key_for(seed, wxid);

        let spec = SearchSpec {
            start: 500000,
            end: 500100,
            wxids: vec![wxid.to_vec()],
            samples: vec![sample(
                "a.png",
                encrypt(&key, b"\x89PNG\r\n\x1a\n0123456789abcdef"),
            )],
        };

        let bar = ProgressBar::hidden();
        assert!(simd::run(&spec, &bar).is_none());
    }

    /// 超长账号标识走标量回落路径, 结果必须一致.
    #[test]
    fn simd_backend_falls_back_for_long_wxid() {
        if !simd::available() {
            return;
        }
        let seed = 777u64;
        let wxid = vec![b'a'; 64];
        let key = key_for(seed, &wxid);

        let spec = SearchSpec {
            start: 770,
            end: 790,
            wxids: vec![wxid.clone()],
            samples: vec![sample(
                "a.png",
                encrypt(&key, b"\x89PNG\r\n\x1a\n0123456789abcdef"),
            )],
        };

        let bar = ProgressBar::hidden();
        let hit = simd::run(&spec, &bar).expect("标量回落应当找到 seed");
        assert_eq!(hit.seed, seed);
        assert_eq!(hit.key, key);
    }

    /// 后端的解析和选择行为.
    #[test]
    fn backend_parsing_and_resolution() {
        assert_eq!("auto".parse::<Backend>().unwrap(), Backend::Auto);
        assert_eq!("cpu".parse::<Backend>().unwrap(), Backend::Cpu);
        assert_eq!("simd".parse::<Backend>().unwrap(), Backend::Simd);
        assert!("gpu".parse::<Backend>().is_err());

        assert_eq!(Backend::Cpu.resolve().unwrap(), Resolved::Cpu);
        let auto = Backend::Auto.resolve().unwrap();
        if simd::available() {
            assert_eq!(auto, Resolved::Simd);
            assert_eq!(Backend::Simd.resolve().unwrap(), Resolved::Simd);
        } else {
            assert_eq!(auto, Resolved::Cpu);
            // 显式要求 simd 时不能静默回落, 必须报错.
            assert!(Backend::Simd.resolve().is_err());
        }
    }
}
