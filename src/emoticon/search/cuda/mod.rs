//! CUDA 后端: 把候选 seed 全丢给 GPU, 命中的候选再回到 CPU 复核.
//!
//! 内核只做筛选, 判定与 `Hit` 构造都走上级模块的 `confirm`, 因此 GPU 与 CPU
//! 两条路径的判定语义完全一致. 这样内核里不需要实现整文件解密和 PKCS#7 校验,
//! 也避免了两套判定逻辑悄悄跑偏.

pub(crate) mod host;

use std::time::{Duration, Instant};

use anyhow::{bail, Context as _, Result};
use indicatif::ProgressBar;
use tracing::{debug, info, warn};

use super::{confirm, Hit, SearchSpec};
use host::{batch_size, Context, DeviceInfo};

/// 单个 MD5 块能容纳的最大消息长度.
const MAX_SINGLE_BLOCK_MESSAGE: usize = 55;

/// 进度日志的最小间隔, 与 wgpu 后端对齐.
///
/// 按批次计数对批次大小敏感, 按时间节流才对扫描范围不敏感.
const PROGRESS_LOG_INTERVAL: Duration = Duration::from_secs(30);

/// 探测 CUDA 是否可用, 返回设备信息.
pub fn probe() -> Result<DeviceInfo> {
    host::probe()
}

/// 在 `spec` 指定的范围内用 GPU 搜索, 找到即返回.
pub fn run(spec: &SearchSpec, bar: &ProgressBar) -> Result<Option<Hit>> {
    check_message_fits(spec)?;

    let context = Context::new().context("初始化 CUDA 上下文失败")?;
    let prefixes = first_blocks(spec)?;
    context
        .upload(&spec.wxids, &prefixes)
        .context("写入 CUDA 常量内存失败")?;

    let total = spec.end - spec.start;
    let batch = batch_size();
    info!(
        batch,
        batches = total.div_ceil(batch),
        candidates = total,
        "CUDA 后端开始搜索"
    );

    let mut position = 0u64;
    // 进度条可见时它本身就是进度, 不必再打日志; 被隐藏 (重定向或 CI) 时才补日志.
    let quiet = !bar.is_hidden();
    let mut last_log = Instant::now();
    while position < total {
        let count = batch.min(total - position);
        let start = spec.start + position;
        let found = context
            .launch(start, count, spec.end)
            .with_context(|| format!("CUDA 批次失败: {start} .. {}", start + count))?;
        position += count;
        bar.set_position(position);

        if !quiet && last_log.elapsed() >= PROGRESS_LOG_INTERVAL {
            last_log = Instant::now();
            info!(done = position, total, "CUDA 搜索进度");
        }

        if !found {
            continue;
        }

        let (seed, wxid_index) = context.hit().context("读取 CUDA 命中结果失败")?;
        let wxid_index = wxid_index as usize;
        debug!(seed, wxid_index, "内核报告候选");
        match confirm(spec, seed, wxid_index) {
            Some(hit) => {
                bar.set_position(total);
                return Ok(Some(hit));
            }
            // 内核与 confirm 的魔数判定一致, 正常不会走到这里. 真的走到时
            // 记一条警告并继续, 而不是把它当成没找到.
            None => warn!(seed, wxid_index, "内核候选未通过 CPU 复核, 继续搜索"),
        }
    }

    bar.set_position(total);
    Ok(None)
}

/// 校验每个 wxid 都能让消息装进单个 MD5 块.
///
/// 内核只实现了单块 MD5, 超出时必须提前报错, 而不是悄悄漏掉一部分候选.
fn check_message_fits(spec: &SearchSpec) -> Result<()> {
    let largest_seed = spec.end.saturating_sub(1).max(spec.start);
    let digits = decimal_len(largest_seed);
    for wxid in &spec.wxids {
        let message = digits + wxid.len() + 8;
        if message > MAX_SINGLE_BLOCK_MESSAGE {
            bail!(
                "CUDA 后端只支持单块 MD5: 最大消息 {message} 字节 (seed {digits} 位 + \
                 wxid {} 字节 + 后缀 8 字节), 上限 {MAX_SINGLE_BLOCK_MESSAGE} 字节, \
                 请改用 --backend cpu",
                wxid.len()
            );
        }
    }
    Ok(())
}

/// 十进制位数, 0 算 1 位.
fn decimal_len(value: u64) -> usize {
    let mut digits = 1;
    let mut rest = value;
    while rest >= 10 {
        rest /= 10;
        digits += 1;
    }
    digits
}

/// 取出每个样本的密文首块.
fn first_blocks(spec: &SearchSpec) -> Result<Vec<[u8; 16]>> {
    let mut prefixes = Vec::with_capacity(spec.samples.len());
    for sample in &spec.samples {
        if sample.prefix.len() < 16 {
            bail!("样本 {} 的首块不足 16 字节", sample.name);
        }
        let mut block = [0u8; 16];
        block.copy_from_slice(&sample.prefix[..16]);
        prefixes.push(block);
    }
    Ok(prefixes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crypto::{derive_key, MAX_MESSAGE};
    use crate::search::testing::three_samples;

    fn spec_for(seed: u64, wxid: &[u8], start: u64, end: u64) -> SearchSpec {
        let mut message = [0u8; MAX_MESSAGE];
        let key = derive_key(seed, wxid, &mut message);
        SearchSpec {
            start,
            end,
            wxids: vec![wxid.to_vec()],
            samples: three_samples(&key),
        }
    }

    #[test]
    fn decimal_len_counts_digits() {
        assert_eq!(decimal_len(0), 1);
        assert_eq!(decimal_len(9), 1);
        assert_eq!(decimal_len(10), 2);
        assert_eq!(decimal_len(4_294_967_295), 10);
        assert_eq!(decimal_len(u64::MAX), 20);
    }

    #[test]
    fn overlong_wxid_is_rejected_before_touching_the_gpu() {
        // 20 位 seed + 32 字节 wxid + 8 字节后缀 = 60 > 55
        let spec = spec_for(7, &[b'w'; 32], u64::MAX - 2, u64::MAX);
        let error = check_message_fits(&spec).unwrap_err().to_string();
        assert!(error.contains("单块 MD5"), "{error}");
    }

    /// 本机是否真能跑 CUDA 内核: 既要有设备, 镜像也要匹配它的计算能力.
    ///
    /// 与 wgpu / metal 的跳过逻辑一致: 没有可用设备时跳过而不是失败,
    /// 因为 CI 上有编译 CUDA 的机器却没有显卡, 那种环境下失败没有诊断价值.
    fn runnable() -> bool {
        host::probe()
            .map(|info| host::image_matches(&info))
            .unwrap_or(false)
    }

    /// 真机测试: 在窄范围里让 GPU 找已知 seed, 并核对派生出的密钥.
    ///
    /// 只在开启 cuda feature 且有可用设备的机器上真正执行.
    #[test]
    fn cuda_backend_finds_known_seed_and_derives_key() {
        if !runnable() {
            eprintln!("跳过 CUDA 正确性测试: 本机没有可用设备或内核镜像与设备不匹配");
            return;
        }

        let seed = 123456789u64;
        let wxid = b"wxid_example123456";
        let mut message = [0u8; MAX_MESSAGE];
        let key = derive_key(seed, wxid, &mut message);

        let spec = spec_for(seed, wxid, seed - 500, seed + 500);
        let bar = ProgressBar::hidden();
        let hit = run(&spec, &bar)
            .expect("CUDA 搜索应当成功")
            .expect("应当找到 seed");

        assert_eq!(hit.seed, seed);
        assert_eq!(hit.wxid, wxid.to_vec());
        assert_eq!(hit.key, key, "GPU 派生出的密钥必须与 CPU 一致");
        assert_eq!(hit.format, "png");
        assert!(hit.padding_ok);
    }

    /// 真机测试: 范围不含已知 seed 时必须报告未命中.
    #[test]
    fn cuda_backend_reports_nothing_outside_range() {
        if !runnable() {
            eprintln!("跳过 CUDA 空范围测试: 本机没有可用设备或内核镜像与设备不匹配");
            return;
        }

        let spec = spec_for(123456789, b"wxid_example123456", 200000000, 200100000);
        let bar = ProgressBar::hidden();
        let hit = run(&spec, &bar).expect("CUDA 搜索应当成功");
        assert!(hit.is_none(), "不该命中 {hit:?}");
    }
}
