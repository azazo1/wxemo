//! SIMD 多路 MD5 后端.
//!
//! 每个候选的消息是 `"<seed>" + wxid + "EMOTICON"`, 长度不超过 20 + 27 + 8 = 55 字节,
//! 填充后正好落在一个 64 字节的 MD5 块里, 因此不需要跨块处理.
//!
//! 同一批里的 seed 连续, 于是:
//!
//! - 批内所有 seed 的十进制位数相同 (批次按位数分段), 消息布局完全一致.
//! - wxid, 后缀, 填充字节和长度域对整批都相同.
//! - 因此 16 个消息字里只有前 `ceil(位数 / 4)` 个需要逐路组装, 其余是整批共用的常量.
//!
//! 批次内部的淘汰顺序和标量路径一致: 先用第一个样本筛掉绝大多数路, 只有首块命中
//! 魔数的路才继续做后续样本的 AES. 最后幸存的路上调用 [`super::scalar::check`],
//! 保证两种后端产出完全相同的 `Hit`.

/// 一个 MD5 块的长度.
const BLOCK: usize = 64;

/// 消息长度上限: 填充的 `0x80` 必须落在长度域之前, 因此消息不能超过 55 字节.
///
/// 超过这个长度的候选无法用单块向量化路径计算, 必须回落标量, 不能静默算错.
const MAX_SINGLE_BLOCK: usize = 55;

/// 向量化路径能接受的最长账号标识: 20 位十进制 + 标识 + 8 字节后缀 <= 55.
const MAX_WXID: usize = MAX_SINGLE_BLOCK - 20 - crate::crypto::SUFFIX.len();

/// 生成一个 MD5 步骤.
///
/// 四个操作都以宏名传入, 这样宏体会在调用点展开, 各平台可以给出自己的指令序列.
/// 对 x86 而言这一点是必须的: AVX2 intrinsic 只有在同一个 `#[target_feature]`
/// 函数体内展开才能被正确内联.
///
/// 定义必须排在各平台子模块的 `mod` 声明之前, `macro_rules` 按文本顺序对子模块可见.
macro_rules! md5_step {
    ($add:ident, $rotl:ident, $f:ident, $dst:ident, $x:ident, $y:ident, $z:ident, $k:expr, $w:expr, $s:literal) => {
        $dst = $add!($dst, $f!($x, $y, $z));
        $dst = $add!($dst, $k);
        $dst = $add!($dst, $w);
        $dst = $rotl!($dst, $s);
        $dst = $add!($dst, $x);
    };
}

#[cfg(target_arch = "aarch64")]
mod aarch64;
#[cfg(target_arch = "x86_64")]
mod x86;

#[cfg(not(any(target_arch = "aarch64", target_arch = "x86_64")))]
mod unsupported;

use std::sync::atomic::{AtomicU64, Ordering};

use indicatif::ProgressBar;
use rayon::prelude::*;
use tracing::{debug, info};

use crate::crypto::{decrypt_first_block, match_magic, new_cipher};

use super::{Hit, SearchSpec, MIN_STRENGTH};

#[cfg(target_arch = "aarch64")]
use aarch64 as imp;
#[cfg(not(any(target_arch = "aarch64", target_arch = "x86_64")))]
use unsupported as imp;
#[cfg(target_arch = "x86_64")]
use x86 as imp;

/// 每批并行路数.
pub const LANES: usize = imp::LANES;

/// 本机是否具备 SIMD 后端所需的指令集.
pub fn available() -> bool {
    imp::available()
}

/// 报错时说明本机缺什么指令集.
pub const REQUIREMENT: &str = imp::REQUIREMENT;

/// 每个任务块覆盖的 seed 数, 与标量后端保持一致, 进度条推进节奏相同.
const CHUNK: u64 = 1 << 20;

/// 每完成这么多任务块输出一次进度日志.
///
/// 只在进度条不可见时才打: 交互运行时条子本身就是进度, 再打日志纯属重复.
/// 但输出被重定向或跑在 CI 里时 indicatif 会隐藏自己, 那时这条日志是唯一的进度来源.
const LOG_EVERY: u64 = 128;

/// 在 `spec` 指定的范围内并行搜索, 找到即返回.
pub fn run(spec: &SearchSpec, bar: &ProgressBar) -> Option<Hit> {
    assert!(
        available(),
        "本机不支持 SIMD 后端, 调用方应先通过 Backend::resolve 检查"
    );

    let targets: Vec<Target> = spec
        .wxids
        .iter()
        .map(|wxid| match Plan::new(wxid) {
            Some(plan) => Target::Vector(Box::new(plan)),
            None => {
                // 消息超过单个 MD5 块时不能向量化, 该账号标识退回标量路径.
                debug!(
                    len = wxid.len(),
                    max = MAX_WXID,
                    "账号标识过长, 改用标量路径"
                );
                Target::Scalar(wxid.clone())
            }
        })
        .collect();
    info!(lanes = LANES, targets = targets.len(), "SIMD 搜索参数");

    let done = AtomicU64::new(0);
    let tasks = AtomicU64::new(0);
    let total = spec.end - spec.start;
    let quiet = !bar.is_hidden();

    let starts: Vec<u64> = (spec.start..spec.end).step_by(CHUNK as usize).collect();
    debug!(tasks = starts.len(), chunk = CHUNK, "任务划分完成");

    let hit = starts.into_par_iter().find_map_any(|start| {
        let end = (start + CHUNK).min(spec.end);
        let found = scan_range(start, end, spec, &targets);
        let finished = done.fetch_add(end - start, Ordering::Relaxed) + (end - start);
        bar.set_position(finished.min(total));
        let count = tasks.fetch_add(1, Ordering::Relaxed) + 1;
        if !quiet && count.is_multiple_of(LOG_EVERY) {
            info!(done = finished, total, "进度");
        }
        found
    });

    bar.set_position(total);
    hit
}

/// 单个账号标识的处理方式.
///
/// 预计算布局有 20 份模板, 校验和的路由表也要装得下, 因此大的一份装箱.
enum Target {
    /// 消息能装进一个 MD5 块, 走向量化路径.
    Vector(Box<Plan>),
    /// 消息过长, 只能逐候选标量处理.
    Scalar(Vec<u8>),
}

/// 扫描 `[start, end)` 区间, 依次尝试每个账号标识.
fn scan_range(start: u64, end: u64, spec: &SearchSpec, targets: &[Target]) -> Option<Hit> {
    for target in targets {
        let found = match target {
            Target::Vector(plan) => scan_plan(start, end, spec, plan),
            Target::Scalar(wxid) => super::scalar::scan_wxid(start, end, spec, wxid),
        };
        if found.is_some() {
            return found;
        }
    }
    None
}

/// 用单个账号标识的预计算布局扫描 `[start, end)`.
fn scan_plan(start: u64, end: u64, spec: &SearchSpec, plan: &Plan) -> Option<Hit> {
    let mut cursor = start;
    while cursor < end {
        // 十进制位数相同的 seed 构成一段连续区间, 段内消息布局一致, 可以整批算.
        let digits = decimal_len(cursor);
        let segment_end = decimal_limit(digits).min(end);
        let mut counter = Decimal::new(cursor);
        let mut seed = cursor;

        // 用 checked_add 判断: 区间上界逼近 u64::MAX 时, 朴素的 seed + LANES 会溢出.
        while seed
            .checked_add(LANES as u64)
            .is_some_and(|next| next <= segment_end)
        {
            let keys = batch_keys(plan, &mut counter);
            if let Some(hit) = evaluate(seed, plan, spec, &keys) {
                return Some(hit);
            }
            seed += LANES as u64;
        }

        // 段尾不足一批的余数交给标量路径, 每段最多 LANES - 1 个候选, 代价可以忽略.
        if seed < segment_end {
            if let Some(hit) = super::scalar::scan_wxid(seed, segment_end, spec, &plan.wxid) {
                return Some(hit);
            }
        }
        cursor = segment_end;
    }
    None
}

/// 逐样本淘汰, 全部样本都通过的路才做整文件复核.
fn evaluate(base: u64, plan: &Plan, spec: &SearchSpec, keys: &[[u8; 16]; LANES]) -> Option<Hit> {
    let mut current: [usize; LANES] = std::array::from_fn(|index| index);
    let mut current_len = LANES;
    let mut next = [0usize; LANES];

    for sample in &spec.samples {
        let mut next_len = 0;
        for &lane in &current[..current_len] {
            let cipher = new_cipher(&keys[lane]);
            let plain = decrypt_first_block(&cipher, &keys[lane], &sample.prefix);
            if match_magic(&plain).is_some_and(|(_, len)| len >= MIN_STRENGTH) {
                next[next_len] = lane;
                next_len += 1;
            }
        }
        current[..next_len].copy_from_slice(&next[..next_len]);
        current_len = next_len;
        if current_len == 0 {
            return None;
        }
    }

    debug!(base, alive = current_len, "整批通过首块判定");
    for &lane in &current[..current_len] {
        let seed = base + lane as u64;
        if let Some(hit) = super::scalar::check(seed, &plan.wxid, &keys[lane], spec) {
            return Some(hit);
        }
    }
    None
}

/// 单个账号标识的预计算布局.
///
/// 数字位数 `d` 决定消息布局, 因此按 `d` 建表: 1 到 20 位各一份 64 字节块模板,
/// 以及对应的 16 个消息字.
struct Plan {
    /// 参与派生的账号标识.
    wxid: Vec<u8>,
    /// 索引 `d` 对应的 64 字节块模板, 下标 0 不用.
    blocks: [[u8; BLOCK]; 21],
    /// 索引 `d` 对应的 16 个消息字 (小端).
    words: [[u32; 16]; 21],
}

impl Plan {
    /// 为账号标识预计算布局. 消息超过单个 MD5 块时返回 `None`.
    fn new(wxid: &[u8]) -> Option<Self> {
        let mut blocks = [[0u8; BLOCK]; 21];
        let mut words = [[0u32; 16]; 21];
        for digits in 1..=20usize {
            let len = digits + wxid.len() + crate::crypto::SUFFIX.len();
            if len > MAX_SINGLE_BLOCK {
                return None;
            }
            let mut block = [0u8; BLOCK];
            block[digits..digits + wxid.len()].copy_from_slice(wxid);
            block[digits + wxid.len()..len].copy_from_slice(crate::crypto::SUFFIX);
            // 补一个 0x80, 后面全是 0, 最后 8 字节放消息的比特长度.
            block[len] = 0x80;
            block[56..64].copy_from_slice(&((len as u64) * 8).to_le_bytes());

            let mut slots = [0u32; 16];
            for (index, slot) in slots.iter_mut().enumerate() {
                *slot = u32::from_le_bytes([
                    block[index * 4],
                    block[index * 4 + 1],
                    block[index * 4 + 2],
                    block[index * 4 + 3],
                ]);
            }
            blocks[digits] = block;
            words[digits] = slots;
        }
        Some(Self {
            wxid: wxid.to_vec(),
            blocks,
            words,
        })
    }
}

/// 一批连续 seed 的候选密钥.
///
/// `counter` 必须停在 `base` 上, 返回时前进 `LANES` 位.
fn batch_keys(plan: &Plan, counter: &mut Decimal) -> [[u8; 16]; LANES] {
    let digits = counter.len;
    // 只有和数字区域重叠的字需要逐路组装, 其余整批相同.
    let varying = digits.div_ceil(4);
    let block = &plan.blocks[digits];
    let common = &plan.words[digits];

    let mut lanes = [[0u8; 20]; LANES];
    for lane in lanes.iter_mut() {
        counter.write_to(lane);
        counter.increment();
    }

    let mut words = [0u32; 16 * LANES];
    for index in 0..16 {
        let target = &mut words[index * LANES..index * LANES + LANES];
        if index < varying {
            for (lane, source) in lanes.iter().enumerate() {
                let mut bytes = [0u8; 4];
                for (offset, byte) in bytes.iter_mut().enumerate() {
                    let position = index * 4 + offset;
                    *byte = if position < digits {
                        source[position]
                    } else {
                        block[position]
                    };
                }
                target[lane] = u32::from_le_bytes(bytes);
            }
        } else {
            target.fill(common[index]);
        }
    }

    let mut keys = [[0u8; 16]; LANES];
    imp::md5_prefix(&words, &mut keys);
    keys
}

/// 十进制位数.
fn decimal_len(value: u64) -> usize {
    if value == 0 {
        1
    } else {
        value.ilog10() as usize + 1
    }
}

/// 该位数区间的上界 (不含). 20 位时超出 u64 表示范围, 直接取 u64::MAX.
fn decimal_limit(digits: usize) -> u64 {
    10u64.checked_pow(digits as u32).unwrap_or(u64::MAX)
}

/// 十进制数字计数器.
///
/// 每批的 seed 是连续的, 因此用逐位递增代替每路一次除法, 省下可观的整数除法开销.
struct Decimal {
    /// 数字字符, 左对齐.
    bytes: [u8; 20],
    /// 当前位数.
    len: usize,
}

impl Decimal {
    /// 用一次除法把数值拆成十进制.
    fn new(value: u64) -> Self {
        let mut bytes = [0u8; 20];
        let mut rest = value;
        let mut len = 0;
        if rest == 0 {
            bytes[0] = b'0';
            len = 1;
        }
        while rest > 0 {
            bytes[len] = b'0' + (rest % 10) as u8;
            rest /= 10;
            len += 1;
        }
        bytes[..len].reverse();
        Self { bytes, len }
    }

    /// 写入数字字符.
    fn write_to(&self, out: &mut [u8]) {
        out[..self.len].copy_from_slice(&self.bytes[..self.len]);
    }

    /// 递增 1, 进位时自动变长.
    ///
    /// 搜索路径按位数分段, 批次不会跨越段边界, 因此实际不会走到进位分支;
    /// 这里仍然处理它, 免得计数器单独使用时悄悄算错.
    fn increment(&mut self) {
        for index in (0..self.len).rev() {
            if self.bytes[index] == b'9' {
                self.bytes[index] = b'0';
            } else {
                self.bytes[index] += 1;
                return;
            }
        }
        // 全是 9: 变成 1 后面跟一串 0. 20 位已到 u64 上限, 不会发生.
        if self.len < self.bytes.len() {
            self.bytes.copy_within(0..self.len, 1);
            self.bytes[0] = b'1';
            self.len += 1;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::testing::key_for;
    use super::*;

    /// 向量化 MD5 必须和标量 MD5 逐字节一致.
    ///
    /// 覆盖各种位数以及 64 位范围的两端. 每批都从段内取, 因为搜索路径按位数分段,
    /// 一批里不会混着两种位数 (那种情况由 `scan_plan` 的分段逻辑处理).
    #[test]
    fn batch_keys_match_scalar_md5() {
        if !available() {
            return;
        }
        let wxid = b"wxid_example123456";
        let plan = Plan::new(wxid).expect("该账号标识应当能向量化");

        let probes: [u64; 11] = [
            0,
            7,
            999,
            1000,
            99999,
            100000,
            4294967295,
            4294967296,
            12345678901234567890,
            u64::MAX - 4,
            u64::MAX - 3,
        ];
        for base in probes {
            let mut counter = Decimal::new(base);
            let keys = batch_keys(&plan, &mut counter);
            let digits = decimal_len(base);
            for (lane, key) in keys.iter().enumerate() {
                // 探测值逼近 u64::MAX, 路数多于 4 时朴素的 base + lane 会溢出.
                let Some(seed) = base.checked_add(lane as u64) else {
                    break;
                };
                // 搜索路径按位数分段, 一批里不会混着两种位数, 这里也只比对同段的路.
                if decimal_len(seed) != digits {
                    break;
                }
                assert_eq!(
                    key,
                    &key_for(seed, wxid),
                    "seed {seed} 的向量化 MD5 结果不一致"
                );
            }
        }
    }

    /// 位数切换处不能因为分段而算错.
    ///
    /// 段边界两侧分别取一批, 覆盖 1 位到 19 位的每个上界.
    #[test]
    fn batch_keys_cross_every_digit_boundary() {
        if !available() {
            return;
        }
        let wxid = b"wxid_abcdefghijkl";
        let plan = Plan::new(wxid).expect("该账号标识应当能向量化");

        for digits in 1..20usize {
            let boundary = decimal_limit(digits);
            // 边界前最后一批和边界后第一批.
            for base in [boundary - LANES as u64, boundary] {
                let mut counter = Decimal::new(base);
                let keys = batch_keys(&plan, &mut counter);
                for (lane, key) in keys.iter().enumerate() {
                    let seed = base + lane as u64;
                    assert_eq!(
                        key,
                        &key_for(seed, wxid),
                        "seed {seed} 的向量化 MD5 结果不一致"
                    );
                }
            }
        }
    }

    /// 数字计数器的行为要和格式化输出一致.
    #[test]
    fn decimal_counter_matches_formatting() {
        let mut counter = Decimal::new(0);
        for value in 0..2000u64 {
            let mut buffer = [0u8; 20];
            counter.write_to(&mut buffer);
            assert_eq!(&buffer[..counter.len], value.to_string().as_bytes());
            counter.increment();
        }

        // 跨越位数变化: 计数器应当自己变长.
        let mut counter = Decimal::new(999_995);
        for value in 999_995..1_000_005u64 {
            let mut buffer = [0u8; 20];
            counter.write_to(&mut buffer);
            assert_eq!(&buffer[..counter.len], value.to_string().as_bytes());
            counter.increment();
        }
    }

    /// 位数区间的上界计算.
    #[test]
    fn decimal_limits_are_exact() {
        assert_eq!(decimal_len(0), 1);
        assert_eq!(decimal_len(9), 1);
        assert_eq!(decimal_len(10), 2);
        assert_eq!(decimal_len(u64::MAX), 20);
        assert_eq!(decimal_limit(1), 10);
        assert_eq!(decimal_limit(19), 10_000_000_000_000_000_000);
        assert_eq!(decimal_limit(20), u64::MAX);
    }

    /// 账号标识过长时必须回落, 不能算出错误的密钥.
    #[test]
    fn plan_rejects_overlong_wxid() {
        assert!(Plan::new(b"wxid_short").is_some());
        // 20 位数字 + 27 字节 wxid + 8 字节后缀 = 55, 刚好还能装进一个块.
        assert!(Plan::new(&[b'a'; MAX_WXID]).is_some());
        // 再多一个字节就必须回落.
        assert!(Plan::new(&[b'a'; MAX_WXID + 1]).is_none());
    }
}
