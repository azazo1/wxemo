//! 多核 seed 搜索.
//!
//! 判定条件: 全部样本用候选密钥解出的首块都命中已知图片魔数.
//!
//! 之所以不依赖 PKCS#7 填充做判定: 微信是否加填充尚未确认, 若它不加填充,
//! 用填充做必要条件会直接漏掉正确候选. 而要求全部样本首块命中魔数本身已经足够强,
//! 3 个样本时最弱的 3 字节 JPEG 魔数也把误报期望压到 1e-5 以下.
//!
//! 填充情况会记录在日志里, 作为诊断信息而不是判定依据.

use std::sync::atomic::{AtomicU64, Ordering};

use aes::Aes128;
use indicatif::ProgressBar;
use rayon::prelude::*;
use tracing::{debug, info};

use crate::derive::{
    decrypt_all, decrypt_first_block, decrypt_last_block, derive_key, match_magic, new_cipher,
    valid_padding, MAX_MESSAGE,
};

/// 每个任务块覆盖的 seed 数.
const CHUNK: u64 = 1 << 20;

/// 判定接受的最小魔数长度. 2 字节的 BMP 太弱, 不作为依据.
const MIN_STRENGTH: usize = 3;

/// 每完成这么多任务块输出一次进度日志, 便于后台运行时观察.
const LOG_EVERY: u64 = 128;

/// 一个样本文件.
pub struct Sample {
    /// 用于展示的名字.
    pub name: String,
    /// 密文首块, 用于魔数判定.
    pub prefix: Vec<u8>,
    /// 密文末块.
    pub tail_last: [u8; 16],
    /// 密文末块之前的那一块.
    pub tail_prev: [u8; 16],
    /// 完整密文, 用于命中后的复核.
    pub body: Vec<u8>,
}

/// 一次搜索的全部输入.
pub struct SearchSpec {
    pub start: u64,
    pub end: u64,
    /// 账号标识候选, 每个都会被尝试.
    pub wxids: Vec<Vec<u8>>,
    pub samples: Vec<Sample>,
}

/// 命中的候选.
#[derive(Debug, Clone)]
pub struct Hit {
    pub seed: u64,
    pub wxid: Vec<u8>,
    pub key: [u8; 16],
    pub format: String,
    /// 命中的样本名与对应格式.
    pub matched: Vec<String>,
    /// 命中时全部样本末块填充是否都合法.
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
        if count % LOG_EVERY == 0 {
            info!(done = finished, total, "进度");
        }
        found
    });

    bar.set_position(total);
    hit
}

/// 扫描 `[start, end)` 区间.
fn scan_range(start: u64, end: u64, spec: &SearchSpec) -> Option<Hit> {
    let mut message = [0u8; MAX_MESSAGE];
    for seed in start..end {
        for wxid in &spec.wxids {
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
                        if best.map_or(true, |(_, best_len)| len > best_len) {
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
            let Some((format, _)) = best else { continue };
            if !all_ok {
                continue;
            }

            // 命中后做一次整文件解密, 确认填充情况并复核首块.
            let mut padding_ok = true;
            for sample in &spec.samples {
                let mut plain = vec![0u8; sample.body.len()];
                decrypt_all(&cipher, &key, &sample.body, &mut plain);
                if match_magic(&plain[..16]).is_none() || !valid_padding(&plain) {
                    padding_ok = false;
                    break;
                }
            }

            info!(
                seed,
                wxid = %String::from_utf8_lossy(wxid),
                format,
                padding_ok,
                matched = ?matched,
                "命中候选"
            );
            return Some(Hit {
                seed,
                wxid: wxid.clone(),
                key,
                format: format.to_string(),
                matched,
                padding_ok,
            });
        }
    }
    None
}

/// 供测试使用: 用给定密钥判定一个样本是否符合预期.
#[allow(dead_code)]
pub fn check(key: &[u8; 16], sample: &Sample) -> bool {
    let cipher = new_cipher(key);
    let plain = decrypt_first_block(&cipher, &key, &sample.prefix);
    match_magic(&plain).is_some()
}

/// 供测试使用: 末块填充是否合法.
#[allow(dead_code)]
pub fn tail_padding(cipher: &Aes128, sample: &Sample) -> bool {
    let tail = decrypt_last_block(cipher, &sample.tail_last, &sample.tail_prev);
    valid_padding(&tail)
}
