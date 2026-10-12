//! Metal 后端的测试.
//!
//! 没有 Metal 设备时全部跳过而不是失败, 这样在 CI 或虚拟机上依然能跑测试.

use indicatif::ProgressBar;

use super::{derive_keys_probe, device_name, run};
use crate::crypto::{derive_key, MAX_MESSAGE};
use crate::emoticon::search::testing::{encrypt, sample, three_samples};
use crate::emoticon::search::SearchSpec;

/// 没有设备时打印跳过原因并返回 true.
macro_rules! skip_without_device {
    () => {
        if device_name().is_none() {
            eprintln!("跳过: 本机没有可用的 Metal 设备");
            return;
        }
    };
}

/// MSL 里的 MD5 必须和 Rust 的 md-5 逐字节一致.
///
/// 覆盖 0, 9 位与 10 位十进制, 以及 32 位与 64 位两种取值, 因为 kernel 里
/// 十进制转换走的是两条不同的路径.
#[test]
fn msl_md5_matches_rust_reference() {
    skip_without_device!();

    let wxid = b"wxid_benchmark123456";
    let seeds: Vec<u64> = vec![
        0,
        1,
        9,
        10,
        99,
        100,
        999_999_999,
        1_000_000_000,
        4_294_967_295,
        4_294_967_296,
        12_345_678_901_234_567_890,
        u64::MAX,
    ];

    // 探针要求范围连续, 所以按段分别派生.
    for &seed in &seeds {
        let keys = derive_keys_probe(seed, 1, wxid).expect("Metal 探针应当能运行");
        let mut message = [0u8; MAX_MESSAGE];
        let expected = derive_key(seed, wxid, &mut message);
        assert_eq!(keys[0], expected, "seed {seed} 的密钥不一致");
    }
}

/// 连续范围内每个候选的密钥都要对得上.
#[test]
fn msl_md5_matches_over_a_contiguous_range() {
    skip_without_device!();

    let wxid = b"wxid_benchmark123456";
    let base = 999_999_990u64;
    let count = 32u64;
    let keys = derive_keys_probe(base, count, wxid).expect("Metal 探针应当能运行");

    let mut message = [0u8; MAX_MESSAGE];
    for offset in 0..count {
        let expected = derive_key(base + offset, wxid, &mut message);
        assert_eq!(
            keys[offset as usize],
            expected,
            "seed {} 的密钥不一致",
            base + offset
        );
    }
}

/// 窄范围内用 Metal 后端找出已知 seed, 并核对派生出的密钥.
#[test]
fn metal_finds_known_seed_in_narrow_range() {
    skip_without_device!();

    let seed = 123_456_789u64;
    let wxid = b"wxid_benchmark123456";
    let mut message = [0u8; MAX_MESSAGE];
    let key = derive_key(seed, wxid, &mut message);

    let spec = SearchSpec {
        start: seed - 40,
        end: seed + 40,
        wxids: vec![wxid.to_vec()],
        samples: three_samples(&key),
    };

    let bar = ProgressBar::hidden();
    let hit = run(&spec, &bar)
        .expect("Metal 后端应当能运行")
        .expect("应当找到 seed");
    assert_eq!(hit.seed, seed);
    assert_eq!(hit.key, key);
    assert_eq!(hit.wxid, wxid.to_vec());
    assert!(hit.padding_ok);
}

/// 范围内没有目标 seed 时不能误报.
#[test]
fn metal_reports_nothing_when_out_of_range() {
    skip_without_device!();

    let seed = 1000u64;
    let wxid = b"wxid_benchmark123456";
    let mut message = [0u8; MAX_MESSAGE];
    let key = derive_key(seed, wxid, &mut message);
    let samples = vec![sample(
        "a.png",
        encrypt(&key, b"\x89PNG\r\n\x1a\n0123456789abcdef"),
    )];

    let spec = SearchSpec {
        start: 5_000_000,
        end: 5_100_000,
        wxids: vec![wxid.to_vec()],
        samples,
    };
    let bar = ProgressBar::hidden();
    assert!(run(&spec, &bar).expect("Metal 后端应当能运行").is_none());
}

/// 多个账号标识写法都要覆盖, 命中应当落在正确的那一个上.
#[test]
fn metal_matches_the_right_wxid_variant() {
    skip_without_device!();

    let seed = 987_654u64;
    let right = b"wxid_target123456";
    let mut message = [0u8; MAX_MESSAGE];
    let key = derive_key(seed, right, &mut message);

    let spec = SearchSpec {
        start: seed - 8,
        end: seed + 8,
        wxids: vec![b"wxid_other1234567".to_vec(), right.to_vec()],
        samples: three_samples(&key),
    };
    let bar = ProgressBar::hidden();
    let hit = run(&spec, &bar)
        .expect("Metal 后端应当能运行")
        .expect("应当找到 seed");
    assert_eq!(hit.seed, seed);
    assert_eq!(hit.wxid, right.to_vec());
}

/// 搜索范围跨过 32 位边界时也要正确.
#[test]
fn metal_handles_range_above_32_bits() {
    skip_without_device!();

    let seed = 4_294_967_300u64;
    let wxid = b"wxid_benchmark123456";
    let mut message = [0u8; MAX_MESSAGE];
    let key = derive_key(seed, wxid, &mut message);

    let spec = SearchSpec {
        start: seed - 4,
        end: seed + 4,
        wxids: vec![wxid.to_vec()],
        samples: three_samples(&key),
    };
    let bar = ProgressBar::hidden();
    let hit = run(&spec, &bar)
        .expect("Metal 后端应当能运行")
        .expect("应当找到 seed");
    assert_eq!(hit.seed, seed);
    assert_eq!(hit.key, key);
}
