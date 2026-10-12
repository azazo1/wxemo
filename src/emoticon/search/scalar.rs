//! 标量后端: 每个候选独立做 MD5 与 AES, 由 rayon 在多个核心上并行.
//!
//! 这是 `--backend cpu` 的实现, 也是其他后端在无法向量化时的回落路径.

use std::sync::atomic::{AtomicU64, Ordering};

use aes::Aes128;
use indicatif::ProgressBar;
use rayon::prelude::*;
use tracing::{debug, info};

use crate::crypto::{
    decrypt_all, decrypt_first_block, derive_key, match_magic, new_cipher, valid_padding,
    MAX_MESSAGE,
};

use super::{Hit, Sample, SearchSpec, MIN_STRENGTH};

/// 每个任务块覆盖的 seed 数.
const CHUNK: u64 = 1 << 20;

/// 每完成这么多任务块输出一次进度日志, 便于后台运行时观察.
const LOG_EVERY: u64 = 128;

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

/// 扫描 `[start, end)` 区间, 依次尝试 `spec.wxids` 里的每个账号标识.
fn scan_range(start: u64, end: u64, spec: &SearchSpec) -> Option<Hit> {
    let mut message = [0u8; MAX_MESSAGE];
    for seed in start..end {
        for wxid in &spec.wxids {
            let key = derive_key(seed, wxid, &mut message);
            if let Some(hit) = check(seed, wxid, &key, spec) {
                return Some(hit);
            }
        }
    }
    None
}

/// 只尝试一个账号标识的标量扫描.
///
/// 与 `scan_range` 的判定完全一致, 供 SIMD 后端在消息超出单个 MD5 块等
/// 无法向量化的情况下回落使用.
pub fn scan_wxid(start: u64, end: u64, spec: &SearchSpec, wxid: &[u8]) -> Option<Hit> {
    let mut message = [0u8; MAX_MESSAGE];
    for seed in start..end {
        let key = derive_key(seed, wxid, &mut message);
        if let Some(hit) = check(seed, wxid, &key, spec) {
            return Some(hit);
        }
    }
    None
}

/// 判定单个候选: 全部样本首块命中魔数才算命中, 命中后再复核整文件填充.
///
/// SIMD 后端在整批路都通过后也调用它, 保证两种后端产出完全相同的 `Hit`.
pub fn check(seed: u64, wxid: &[u8], key: &[u8; 16], spec: &SearchSpec) -> Option<Hit> {
    let cipher = new_cipher(key);

    // 判定: 每个样本的首块都必须命中已知魔数.
    let mut best: Option<(&'static str, usize)> = None;
    let mut matched = Vec::new();
    let mut all_ok = true;
    for sample in &spec.samples {
        let plain = decrypt_first_block(&cipher, key, &sample.prefix);
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

/// 判定单个候选是否让样本首块命中魔数.
#[cfg_attr(not(test), allow(dead_code))]
pub fn matches(key: &[u8; 16], sample: &Sample) -> bool {
    let cipher: Aes128 = new_cipher(key);
    let plain = decrypt_first_block(&cipher, key, &sample.prefix);
    match_magic(&plain).is_some()
}

#[cfg(test)]
mod tests {
    use super::super::testing::{encrypt, key_for, sample};
    use super::*;

    #[test]
    fn search_finds_known_seed_across_formats() {
        let seed = 87654321u64;
        let wxid = b"wxid_example123456";
        let key = key_for(seed, wxid);

        let samples = vec![
            sample("a.png", encrypt(&key, b"\x89PNG\r\n\x1a\n0123456789abcdef")),
            sample("b.gif", encrypt(&key, b"GIF89a0123456789abcdef")),
            sample("c.jpg", encrypt(&key, b"\xff\xd8\xff\xe00123456789abcdef")),
        ];

        let spec = SearchSpec {
            start: seed - 3,
            end: seed + 3,
            wxids: vec![wxid.to_vec()],
            samples,
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
        let key = key_for(seed, wxid);
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
        let key = key_for(seed, b"wxid_correct");
        let s = sample("a.png", encrypt(&key, b"\x89PNG\r\n\x1a\n0123456789abcdef"));

        let wrong = key_for(seed, b"wxid_wrong");
        assert!(!matches(&wrong, &s));
        assert!(matches(&key, &s));
    }

    /// 单账号扫描入口必须和整表扫描结果一致.
    #[test]
    fn scan_wxid_agrees_with_scan_range() {
        let seed = 1234u64;
        let wxid = b"wxid_example123456";
        let key = key_for(seed, wxid);
        let spec = SearchSpec {
            start: 1230,
            end: 1240,
            wxids: vec![wxid.to_vec()],
            samples: vec![sample(
                "a.png",
                encrypt(&key, b"\x89PNG\r\n\x1a\n0123456789abcdef"),
            )],
        };

        let by_range = scan_range(spec.start, spec.end, &spec).expect("整表扫描应当命中");
        let by_wxid = scan_wxid(spec.start, spec.end, &spec, wxid).expect("单账号扫描应当命中");
        assert_eq!(by_range.seed, by_wxid.seed);
        assert_eq!(by_range.key, by_wxid.key);
    }
}
