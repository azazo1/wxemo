//! 多核 seed 搜索.
//!
//! 判定条件: 全部样本用候选密钥解出的首块都命中已知图片魔数.
//!
//! 之所以不依赖 PKCS#7 填充做判定: 微信是否加填充不便先验假定, 若它不加填充,
//! 用填充做必要条件会直接漏掉正确候选. 而要求全部样本首块命中魔数本身已经足够强,
//! 3 个样本时最弱的 3 字节 JPEG 魔数也把误报期望压到 1e-5 以下.
//!
//! 填充情况会记录在日志与结果里, 作为诊断信息而不是判定依据.

use std::sync::atomic::{AtomicU64, Ordering};

use aes::Aes128;
use indicatif::ProgressBar;
use rayon::prelude::*;
use tracing::{debug, info};

use crate::crypto::{
    decrypt_all, decrypt_first_block, derive_key, match_magic, new_cipher, valid_padding,
    MAX_MESSAGE,
};

/// 每个任务块覆盖的 seed 数.
const CHUNK: u64 = 1 << 20;

/// 判定接受的最小魔数长度. 2 字节的魔数太弱, 不作为依据.
const MIN_STRENGTH: usize = 3;

/// 每完成这么多任务块输出一次进度日志, 便于后台运行时观察.
const LOG_EVERY: u64 = 128;

/// 一个样本文件.
#[derive(Clone)]
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

/// 在 `spec` 指定的范围内并行搜索, 找到即返回.
pub fn run(spec: &SearchSpec, bar: &ProgressBar) -> Option<Hit> {
    let done = AtomicU64::new(0);
    let tasks = AtomicU64::new(0);
    let total = spec.end - spec.start;

    let starts: Vec<u64> = (spec.start..spec.end).step_by(CHUNK as usize).collect();
    debug!(tasks = starts.len(), chunk = CHUNK, "任务划分完成");

    let hit = starts.into_par_iter().find_map_any(|start| {
        let end = (start + CHUNK).min(spec.end);
        let found = scan_range(start, end, spec);
        let finished = done.fetch_add(end - start, Ordering::Relaxed) + (end - start);
        bar.set_position(finished.min(total));
        let count = tasks.fetch_add(1, Ordering::Relaxed) + 1;
        if count.is_multiple_of(LOG_EVERY) {
            info!(done = finished, total, "进度");
        }
        found
    });

    bar.set_position(total);
    hit
}

/// 扫描 `[start, end)` 区间.
fn scan_range(start: u64, end: u64, spec: &SearchSpec) -> Option<Hit> {
    for seed in start..end {
        for wxid in &spec.wxids {
            if let Some(hit) = verify_candidate(spec, seed, wxid) {
                return Some(hit);
            }
        }
    }
    None
}

/// 判定单个候选, 命中则返回完整的 `Hit`.
///
/// GPU 后端在着色器报出候选之后也走这里复核, 保证两个后端的判定条件逐字一致.
pub fn verify_candidate(spec: &SearchSpec, seed: u64, wxid: &[u8]) -> Option<Hit> {
    let mut message = [0u8; MAX_MESSAGE];
    let key = derive_key(seed, wxid, &mut message);
    let cipher = new_cipher(&key);

    // 判定: 每个样本的首块都必须命中已知魔数.
    let mut best: Option<(&'static str, usize)> = None;
    let mut matched = Vec::new();
    let mut all_ok = true;
    for sample in &spec.samples {
        let plain = decrypt_first_block(&cipher, &key, &sample.prefix);
        match match_magic(&plain) {
            Some((name, len)) if len >= MIN_STRENGTH => {
                if best.is_none_or(|(_, best_len)| len > best_len) {
                    best = Some((name, len));
                }
                matched.push(format!("{}:{}", sample.name, name));
            }
            _ => {
                all_ok = false;
                break;
            }
        }
    }
    let (format, _) = best?;
    if !all_ok {
        return None;
    }

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
    let cipher: Aes128 = new_cipher(key);
    let plain = decrypt_first_block(&cipher, key, &sample.prefix);
    match_magic(&plain).is_some()
}

/// 构造合成样本的测试辅助, 供 CPU 与 GPU 两侧的测试共用.
#[cfg(test)]
pub mod test_support {
    use super::*;

    /// 用密钥加密明文 (PKCS#7 填充), 便于构造测试样本.
    pub fn encrypt(key: &[u8; 16], plain: &[u8]) -> Vec<u8> {
        use aes::cipher::BlockEncrypt;
        use aes::cipher::KeyInit;

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

    /// 把一个密文整体包成样本.
    pub fn sample(name: &str, body: Vec<u8>) -> Sample {
        Sample {
            name: name.to_string(),
            prefix: body[..16].to_vec(),
            body,
        }
    }

    /// 用给定密钥派生一组覆盖 PNG / GIF / JPEG 的样本.
    pub fn format_samples(key: &[u8; 16]) -> Vec<Sample> {
        vec![
            sample("a.png", encrypt(key, b"\x89PNG\r\n\x1a\n0123456789abcdef")),
            sample("b.gif", encrypt(key, b"GIF89a0123456789abcdef")),
            sample("c.jpg", encrypt(key, b"\xff\xd8\xff\xe00123456789abcdef")),
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::{encrypt, format_samples, sample};
    use super::*;

    #[test]
    fn search_finds_known_seed_across_formats() {
        let seed = 87654321u64;
        let wxid = b"wxid_example123456";
        let mut message = [0u8; MAX_MESSAGE];
        let key = derive_key(seed, wxid, &mut message);

        let spec = SearchSpec {
            start: seed - 3,
            end: seed + 3,
            wxids: vec![wxid.to_vec()],
            samples: format_samples(&key),
        };
        let bar = ProgressBar::hidden();
        let hit = run(&spec, &bar).expect("应当找到 seed");
        assert_eq!(hit.seed, seed);
        assert_eq!(hit.wxid, wxid.to_vec());
        assert_eq!(hit.key, key);
        assert!(hit.padding_ok);
    }

    #[test]
    fn search_reports_nothing_when_out_of_range() {
        let seed = 1000u64;
        let wxid = b"wxid_example123456";
        let mut message = [0u8; MAX_MESSAGE];
        let key = derive_key(seed, wxid, &mut message);
        let samples = vec![sample(
            "a.png",
            encrypt(&key, b"\x89PNG\r\n\x1a\n0123456789abcdef"),
        )];

        let spec = SearchSpec {
            start: 5000,
            end: 5004,
            wxids: vec![wxid.to_vec()],
            samples,
        };
        let bar = ProgressBar::hidden();
        assert!(run(&spec, &bar).is_none());
    }

    #[test]
    fn wrong_wxid_does_not_match() {
        let seed = 4242u64;
        let mut message = [0u8; MAX_MESSAGE];
        let key = derive_key(seed, b"wxid_correct", &mut message);
        let s = sample("a.png", encrypt(&key, b"\x89PNG\r\n\x1a\n0123456789abcdef"));

        let mut other = [0u8; MAX_MESSAGE];
        let wrong = derive_key(seed, b"wxid_wrong", &mut other);
        assert!(!matches(&wrong, &s));
        assert!(matches(&key, &s));
    }
}
