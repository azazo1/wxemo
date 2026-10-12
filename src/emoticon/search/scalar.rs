//! CPU 后端: 基于 rayon 的多核扫描.
//!
//! 这是最初的实现, 语义和行为保持不变, 仍然作为 `--backend cpu` 的实现.

use std::sync::atomic::{AtomicU64, Ordering};

use indicatif::ProgressBar;
use rayon::prelude::*;
use tracing::{debug, info};

use super::{verify_hit, Hit, SearchSpec};

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

/// 扫描 `[start, end)` 区间.
fn scan_range(start: u64, end: u64, spec: &SearchSpec) -> Option<Hit> {
    for seed in start..end {
        for wxid in &spec.wxids {
            if let Some(hit) = verify_hit(seed, wxid, spec) {
                return Some(hit);
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crypto::{derive_key, MAX_MESSAGE};
    use crate::emoticon::search::testutil::{encrypt, sample, three_samples};

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
            samples: three_samples(&key),
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
        assert!(!super::super::matches(&wrong, &s));
        assert!(super::super::matches(&key, &s));
    }
}
