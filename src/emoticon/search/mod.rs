//! seed 搜索的公共类型与后端调度.
//!
//! 判定条件: 全部样本用候选密钥解出的首块都命中已知图片魔数.
//!
//! 之所以不依赖 PKCS#7 填充做判定: 微信是否加填充不便先验假定, 若它不加填充,
//! 用填充做必要条件会直接漏掉正确候选. 而要求全部样本首块命中魔数本身已经足够强,
//! 3 个样本时最弱的 3 字节 JPEG 魔数也把误报期望压到 1e-5 以下.
//!
//! 填充情况会记录在日志与结果里, 作为诊断信息而不是判定依据.
//!
//! 后端可选 CPU (rayon 多核) 与 macOS 上的 Metal compute. 两者共用同一段
//! 命中判定与命中后的整文件复核, 因此搜索结果只可能在速度上有差别.

use anyhow::Result;
use indicatif::ProgressBar;
use tracing::info;

use crate::crypto::{
    decrypt_all, decrypt_first_block, derive_key, match_magic, new_cipher, valid_padding,
    MAX_MESSAGE,
};

pub mod scalar;

#[cfg(target_os = "macos")]
pub mod metal;

/// 判定接受的最小魔数长度. 2 字节的魔数太弱, 不作为依据.
pub const MIN_STRENGTH: usize = 3;

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

/// 搜索使用的计算后端.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Backend {
    /// 能用 Metal 就用 Metal, 否则回落 CPU.
    #[default]
    Auto,
    /// 只用 CPU.
    Cpu,
    /// 只用 Metal, 不可用时明确报错.
    Metal,
}

/// 在 `spec` 指定的范围内搜索, 找到即返回.
///
/// `Auto` 会选择可用的后端, 并用日志说明实际选了哪个和哪个设备.
pub fn run(spec: &SearchSpec, bar: &ProgressBar, backend: Backend) -> Result<Option<Hit>> {
    match backend {
        Backend::Cpu => {
            info!(backend = "cpu", "使用 CPU 后端");
            Ok(scalar::run(spec, bar))
        }
        Backend::Metal => run_metal(spec, bar),
        Backend::Auto => {
            #[cfg(target_os = "macos")]
            if let Some(name) = metal::device_name() {
                info!(backend = "metal", device = %name, "自动选择 Metal 后端");
                return metal::run(spec, bar);
            }
            info!(backend = "cpu", "没有可用的 Metal 设备, 回落 CPU 后端");
            Ok(scalar::run(spec, bar))
        }
    }
}

/// 显式请求 Metal 后端.
fn run_metal(spec: &SearchSpec, bar: &ProgressBar) -> Result<Option<Hit>> {
    #[cfg(target_os = "macos")]
    {
        let name = metal::device_name()
            .ok_or_else(|| anyhow::anyhow!("没有可用的 Metal 设备, 无法使用 metal 后端"))?;
        info!(backend = "metal", device = %name, "使用 Metal 后端");
        metal::run(spec, bar)
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = (spec, bar);
        anyhow::bail!("当前平台不支持 Metal 后端, 请改用 --backend cpu 或 --backend auto")
    }
}

/// 判定单个候选: 全部样本首块命中魔数才算命中, 命中后做整文件复核.
///
/// 这是两个后端的共同判定入口, GPU 只负责把可能是候选的 seed 筛出来.
pub fn verify_hit(seed: u64, wxid: &[u8], spec: &SearchSpec) -> Option<Hit> {
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
        wxid: wxid.to_vec(),
        key,
        format: format.to_string(),
        matched,
        padding_ok,
    })
}

/// 判定单个候选是否让样本首块命中魔数.
#[cfg_attr(not(test), allow(dead_code))]
pub fn matches(key: &[u8; 16], sample: &Sample) -> bool {
    let cipher = new_cipher(key);
    let plain = decrypt_first_block(&cipher, key, &sample.prefix);
    match_magic(&plain).is_some()
}

#[cfg(test)]
pub(crate) mod testutil {
    use super::*;

    use aes::cipher::BlockEncrypt;
    use aes::cipher::KeyInit;
    use aes::Aes128;

    /// 用密钥加密明文 (PKCS#7 填充), 便于构造测试样本.
    pub fn encrypt(key: &[u8; 16], plain: &[u8]) -> Vec<u8> {
        let padding = 16 - plain.len() % 16;
        let mut padded = plain.to_vec();
        padded.extend(std::iter::repeat_n(padding as u8, padding));

        let cipher = Aes128::new(aes::cipher::generic_array::GenericArray::from_slice(key));
        let mut previous = *key;
        let mut out = Vec::with_capacity(padded.len());
        for chunk in padded.as_chunks::<16>().0 {
            let mut block = aes::cipher::generic_array::GenericArray::clone_from_slice(chunk);
            for index in 0..16 {
                block[index] ^= previous[index];
            }
            cipher.encrypt_block(&mut block);
            out.extend_from_slice(&block);
            previous.copy_from_slice(&block);
        }
        out
    }

    /// 由完整密文构造样本.
    pub fn sample(name: &str, body: Vec<u8>) -> Sample {
        Sample {
            name: name.to_string(),
            prefix: body[..16].to_vec(),
            body,
        }
    }

    /// 三个常见格式 (PNG / GIF / JPEG) 各一个样本.
    pub fn three_samples(key: &[u8; 16]) -> Vec<Sample> {
        vec![
            sample("a.png", encrypt(key, b"\x89PNG\r\n\x1a\n0123456789abcdef")),
            sample("b.gif", encrypt(key, b"GIF89a0123456789abcdef")),
            sample("c.jpg", encrypt(key, b"\xff\xd8\xff\xe00123456789abcdef")),
        ]
    }
}
