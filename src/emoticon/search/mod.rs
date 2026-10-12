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
//! 各后端的判定条件完全一致, 区别只在并行粒度:
//!
//! - `scalar`: 每个候选独立做标量 MD5 与 AES, 由 rayon 在多个核心上并行.
//! - `simd`: 一批候选共用一次向量化 MD5, 每路各自做 AES 首块判定.
//! - `cuda`: GPU 只筛选候选, 命中后回到 `confirm` 复核 (需要 `cuda` feature).
//! - `metal`: macOS 原生 Metal compute, 同样只筛选, 复核走 `confirm`.
//! - `gpu`: wgpu 跨平台 GPU 后端, 一套 WGSL 覆盖 Metal / Vulkan / DX12.
//!
//! 选路由 [`Backend::resolve`] 决定, 显式指定某个后端而本机不支持时直接报错,
//! 不会静默退回标量路径.

pub(crate) mod scalar;
mod simd;

#[cfg(feature = "cuda")]
pub(crate) mod cuda;

pub(crate) mod gpu;

#[cfg(target_os = "macos")]
pub(crate) mod metal;

#[cfg(test)]
pub(crate) mod testing;

use std::fmt;
use std::str::FromStr;

use anyhow::{bail, Result};
use indicatif::ProgressBar;
use tracing::info;

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
    /// 按本机能力自动选择: CUDA 优先, 其次 Metal, 再次 SIMD, 最后标量.
    #[default]
    Auto,
    /// 标量路径, 只靠多核并行.
    Cpu,
    /// 多路向量化路径.
    Simd,
    /// CUDA 后端, 不可用时明确报错.
    Cuda,
    /// macOS Metal 后端, 不可用时明确报错.
    Metal,
    /// wgpu 跨平台 GPU 后端, 不可用时明确报错.
    Wgpu,
}

impl FromStr for Backend {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "auto" => Ok(Backend::Auto),
            "cpu" => Ok(Backend::Cpu),
            "simd" => Ok(Backend::Simd),
            "cuda" => Ok(Backend::Cuda),
            "metal" => Ok(Backend::Metal),
            "wgpu" => Ok(Backend::Wgpu),
            other => Err(format!(
                "未知后端 {other}, 可选 auto / cpu / simd / cuda / metal / wgpu"
            )),
        }
    }
}

impl fmt::Display for Backend {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let name = match self {
            Backend::Auto => "auto",
            Backend::Cpu => "cpu",
            Backend::Simd => "simd",
            Backend::Cuda => "cuda",
            Backend::Metal => "metal",
            Backend::Wgpu => "wgpu",
        };
        formatter.write_str(name)
    }
}

impl Backend {
    /// 结合本机能力确定实际使用的后端.
    ///
    /// `auto` 按能力自动选择; 显式指定某个后端而能力不足时返回错误,
    /// 避免用户以为在用加速后端而实际跑的是标量.
    pub fn resolve(self) -> Result<Resolved> {
        match self {
            Backend::Cpu => Ok(Resolved::Cpu),
            Backend::Simd if simd::available() => Ok(Resolved::Simd),
            Backend::Simd => bail!(
                "本机不支持 simd 后端, 需要 {}; 可改用 --backend cpu",
                simd::REQUIREMENT
            ),
            Backend::Cuda => match cuda_available() {
                Ok(()) => Ok(Resolved::Cuda),
                Err(reason) => bail!("指定了 --backend cuda 但 CUDA 不可用: {reason:#}"),
            },
            Backend::Metal => match metal_available() {
                Some(()) => Ok(Resolved::Metal),
                None => bail!("指定了 --backend metal 但本机没有可用的 Metal 设备"),
            },
            Backend::Wgpu => match gpu_available() {
                Some(()) => Ok(Resolved::Wgpu),
                None => bail!("指定了 --backend wgpu 但本机没有可用的 GPU adapter"),
            },
            Backend::Auto => {
                // GPU 优先: CUDA 的算力上限最高, 其次是原生 Metal,
                // 再退跨平台的 wgpu, 最后才是 CPU 路径.
                if cuda_available().is_ok() {
                    info!("检测到可用 CUDA 设备, 使用 cuda 后端");
                    return Ok(Resolved::Cuda);
                }
                if metal_available().is_some() {
                    info!("使用 metal 后端");
                    return Ok(Resolved::Metal);
                }
                if gpu_available().is_some() {
                    info!("使用 wgpu 后端");
                    return Ok(Resolved::Wgpu);
                }
                if simd::available() {
                    info!("使用 simd 后端");
                    return Ok(Resolved::Simd);
                }
                info!("使用 cpu 后端");
                Ok(Resolved::Cpu)
            }
        }
    }
}

/// 探测 wgpu adapter.
fn gpu_available() -> Option<()> {
    gpu::available().then_some(())
}

/// 探测 Metal 设备.
#[cfg(target_os = "macos")]
fn metal_available() -> Option<()> {
    metal::device_name().map(|name| {
        info!(device = %name, "Metal 设备就绪");
    })
}

/// 探测 Metal 设备.
#[cfg(not(target_os = "macos"))]
fn metal_available() -> Option<()> {
    None
}

/// 检查 CUDA 是否可用.
///
/// 除了设备本身, 还要确认随二进制发布的内核镜像能在这个设备上加载.
/// 镜像只在对应计算能力的设备上有效, 在这里挡住, `auto` 才能回落到别的后端.
#[cfg(feature = "cuda")]
fn cuda_available() -> Result<()> {
    let info = cuda::probe()?;
    if !cuda::image_matches(&info) {
        bail!(
            "设备计算能力 {}.{} 不在内核覆盖范围 sm_{} 内",
            info.major,
            info.minor,
            cuda::TARGET_SMS
        );
    }
    info!(
        device = %info.name,
        compute_capability = %format!("{}.{}", info.major, info.minor),
        "CUDA 设备就绪"
    );
    Ok(())
}

/// 检查 CUDA 是否可用.
#[cfg(not(feature = "cuda"))]
fn cuda_available() -> Result<()> {
    bail!("本二进制编译时没有开启 cuda feature, 请用 `cargo build --features cuda` 重新编译")
}

/// 实际执行搜索的后端.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Resolved {
    /// 标量多核.
    Cpu,
    /// SIMD 多路.
    Simd,
    /// CUDA.
    Cuda,
    /// macOS Metal.
    Metal,
    /// wgpu 跨平台 GPU.
    Wgpu,
}

impl fmt::Display for Resolved {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let name = match self {
            Resolved::Cpu => "cpu",
            Resolved::Simd => "simd",
            Resolved::Cuda => "cuda",
            Resolved::Metal => "metal",
            Resolved::Wgpu => "wgpu",
        };
        formatter.write_str(name)
    }
}

impl Resolved {
    /// 该后端一次并行处理的候选数. 非向量化后端为 1.
    pub fn lanes(self) -> usize {
        match self {
            Resolved::Cpu => 1,
            Resolved::Simd => simd::LANES,
            Resolved::Cuda => 1,
            Resolved::Metal => 1,
            Resolved::Wgpu => 1,
        }
    }
}

/// 按已解析的后端执行搜索, 找到即返回.
pub fn run(spec: &SearchSpec, bar: &ProgressBar, backend: Resolved) -> Result<Option<Hit>> {
    info!(backend = %backend, lanes = backend.lanes(), "搜索后端");
    match backend {
        Resolved::Cpu => Ok(scalar::run(spec, bar)),
        Resolved::Simd => Ok(simd::run(spec, bar)),
        #[cfg(feature = "cuda")]
        Resolved::Cuda => cuda::run(spec, bar),
        #[cfg(not(feature = "cuda"))]
        Resolved::Cuda => bail!("本二进制编译时没有开启 cuda feature"),
        #[cfg(target_os = "macos")]
        Resolved::Metal => metal::run(spec, bar),
        #[cfg(not(target_os = "macos"))]
        Resolved::Metal => bail!("metal 后端只在 macOS 上可用"),
        Resolved::Wgpu => gpu::run(spec, bar),
    }
}

/// 复核一个候选: 全部样本首块都命中魔数才算命中, 命中后整文件解密确认填充情况.
///
/// GPU 内核只负责把候选筛出来, 真正的判定与 `Hit` 的构造都回到这里,
/// 保证各后端的判定语义完全一致.
pub(crate) fn confirm(spec: &SearchSpec, seed: u64, wxid_index: usize) -> Option<Hit> {
    let wxid = spec.wxids.get(wxid_index)?;
    let mut message = [0u8; MAX_MESSAGE];
    let key = derive_key(seed, wxid, &mut message);
    verify_hit(seed, wxid, &key, spec)
}

/// 用给定密钥复核一个候选, 命中则构造 `Hit`.
pub(crate) fn verify_hit(seed: u64, wxid: &[u8], key: &[u8; 16], spec: &SearchSpec) -> Option<Hit> {
    let cipher = new_cipher(key);

    let mut best: Option<(&'static str, usize)> = None;
    let mut matched = Vec::new();
    for sample in &spec.samples {
        let plain = decrypt_first_block(&cipher, key, &sample.prefix);
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
        decrypt_all(&cipher, key, &sample.body, &mut plain);
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
        wxid: wxid.to_vec(),
        key: *key,
        format: format.to_string(),
        matched,
        padding_ok,
    })
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
        assert_eq!("cuda".parse::<Backend>().unwrap(), Backend::Cuda);
        assert_eq!("metal".parse::<Backend>().unwrap(), Backend::Metal);
        assert_eq!("wgpu".parse::<Backend>().unwrap(), Backend::Wgpu);
        assert!("vulkan".parse::<Backend>().is_err());
        assert_eq!(Backend::default(), Backend::Auto);
        assert_eq!(Backend::Cuda.to_string(), "cuda");
        assert_eq!(Backend::Wgpu.to_string(), "wgpu");
        assert_eq!(Resolved::Cpu.to_string(), "cpu");

        assert_eq!(Backend::Cpu.resolve().unwrap(), Resolved::Cpu);

        if simd::available() {
            assert_eq!(Backend::Simd.resolve().unwrap(), Resolved::Simd);
        } else {
            // 显式要求 simd 时不能静默回落, 必须报错.
            assert!(Backend::Simd.resolve().is_err());
        }
    }

    /// 没有 cuda feature 时, 显式指定 cuda 必须报错而不是静默回落.
    #[cfg(not(feature = "cuda"))]
    #[test]
    fn explicit_cuda_without_feature_errors() {
        let error = Backend::Cuda.resolve().unwrap_err().to_string();
        assert!(error.contains("cuda feature"), "{error}");
    }

    /// 没有 cuda feature 时 auto 必须仍然可用.
    #[cfg(not(feature = "cuda"))]
    #[test]
    fn auto_works_without_cuda_feature() {
        assert!(matches!(
            Backend::Auto.resolve().unwrap(),
            Resolved::Cpu | Resolved::Simd | Resolved::Metal | Resolved::Wgpu
        ));
    }
}
