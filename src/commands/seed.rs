//! `wxemo seed search`: 暴力搜索账号的 seed.

pub mod manage;
pub mod transfer;

use std::fs;
use std::path::{Path, PathBuf};
use std::time::Instant;

use anyhow::{bail, Context, Result};
use indicatif::{ProgressBar, ProgressStyle};
use tracing::{debug, info};

use crate::config::{Auth, Config};
use crate::crypto::MAX_MESSAGE;
use crate::discover;
use crate::search::{run, Backend, Sample, SearchSpec};

/// 搜索参数.
pub struct SeedOptions {
    /// 显式指定的账号 (wxid 或目录名).
    pub account: Option<String>,
    /// 直接指定表情包密文目录, 跳过账号发现.
    pub emoticon_dir: Option<PathBuf>,
    /// 额外/替代的数据根目录.
    pub data_roots: Vec<String>,
    /// 搜索起点.
    pub start: Option<u64>,
    /// 搜索终点.
    pub end: Option<u64>,
    /// 线程数, 0 表示全部核心.
    pub threads: Option<usize>,
    /// 用于判定的样本数量.
    pub sample_count: usize,
    /// 计算后端.
    pub backend: Backend,
}

/// 执行 seed 搜索.
pub fn run_seed(options: SeedOptions, interactive: bool) -> Result<()> {
    let (config, _) = Config::load()?;
    let mut auth = Auth::load()?;

    // 1. 确定要搜的账号标识与密文目录.
    let (wxid, emoticon_dir) = resolve_target(&options, &config, interactive)?;
    info!(wxid, dir = %emoticon_dir.display(), "搜索目标");

    if let Some(existing) = auth.get(&wxid) {
        info!(seed = existing.seed, key = %existing.key, "该账号已有记录, 仍会重新搜索");
    }

    // 2. 挑选样本.
    let samples = pick_samples(&emoticon_dir, options.sample_count)?;
    info!(count = samples.len(), "载入判定样本");
    for sample in &samples {
        debug!(name = %sample.name, len = sample.body.len(), "样本");
    }

    // 3. 范围与线程.
    let start = options.start.unwrap_or(config.search.start);
    let end = options.end.unwrap_or(config.search.end);
    if start >= end {
        bail!("搜索范围为空: {start} .. {end}");
    }
    let threads = options.threads.unwrap_or(config.search.threads);
    if threads > 0 {
        let _ = rayon::ThreadPoolBuilder::new()
            .num_threads(threads)
            .build_global();
    }

    let total = end - start;
    info!(
        start,
        end,
        candidates = total,
        threads = if threads == 0 {
            rayon::current_num_threads()
        } else {
            threads
        },
        "开始搜索"
    );

    let bar = ProgressBar::new(total);
    bar.set_style(
        ProgressStyle::with_template(
            "{spinner:.green} {percent:>5.1}%  {per_sec:>12}  已用 {elapsed:>8}  剩余 {eta:>8}  [{bar:28}]",
        )
        .expect("进度条模板非法")
        .progress_chars("=> "),
    );

    // 账号标识的两种写法都试: 带后缀与不带后缀.
    // 二进制的证据指向不带后缀, 但多试一种代价很小, 能避免因版本差异而搜不到.
    let mut wxids = vec![wxid.clone().into_bytes()];
    let folder_variant = folder_name_variant(&options, &config, &wxid, interactive);
    if let Some(variant) = folder_variant {
        if variant != wxid {
            info!(variant, "同时尝试带后缀的目录名写法");
            wxids.push(variant.into_bytes());
        }
    }

    let spec = SearchSpec {
        start,
        end,
        wxids,
        samples,
    };

    let started = Instant::now();
    let hit = run(&spec, &bar, options.backend)?;
    bar.finish_and_clear();
    let elapsed = started.elapsed();

    let Some(hit) = hit else {
        bail!(
            "在 {start} .. {end} 范围内没有找到 {wxid} 的 seed, 用时 {:.3} 秒",
            elapsed.as_secs_f64()
        );
    };

    let rate = total as f64 / elapsed.as_secs_f64();
    info!(
        seed = hit.seed,
        wxid = %String::from_utf8_lossy(&hit.wxid),
        key = %crate::config::hex(&hit.key),
        format = %hit.format,
        padding_ok = hit.padding_ok,
        seconds = elapsed.as_secs_f64(),
        rate = rate as u64,
        "命中"
    );

    let matched_wxid = String::from_utf8_lossy(&hit.wxid).to_string();
    println!("seed  = {}", hit.seed);
    println!("wxid  = {matched_wxid}");
    println!("key   = {}", crate::config::hex(&hit.key));
    println!("格式  = {}", hit.format);
    println!("填充合法 = {}", hit.padding_ok);
    println!("命中样本 = {}", hit.matched.join(", "));
    println!(
        "用时  = {:.1} 秒 ({:.0} 候选/秒)",
        elapsed.as_secs_f64(),
        rate
    );

    auth.put(&matched_wxid, hit.seed, &hit.key);
    auth.save().context("写入密钥文件失败")?;
    println!("已记录到 {}", crate::config::auth_path().display());
    Ok(())
}

/// 确定账号标识与密文目录.
fn resolve_target(
    options: &SeedOptions,
    config: &Config,
    interactive: bool,
) -> Result<(String, PathBuf)> {
    if let Some(dir) = &options.emoticon_dir {
        if !dir.is_dir() {
            bail!("表情包目录不存在: {}", dir.display());
        }
        // 目录形如 .../xwechat_files/<账号>/business/emoticon
        let folder = dir
            .parent()
            .and_then(|p| p.parent())
            .and_then(|p| p.file_name())
            .map(|name| name.to_string_lossy().to_string())
            .context("无法从目录结构推断账号, 请改用 --account")?;
        let wxid = discover::normalize_wxid(&folder);
        return Ok((wxid, dir.clone()));
    }

    let roots = discover::defaults::resolve(&options.data_roots, &config.data_roots)?;
    if roots.is_empty() {
        bail!("尚未配置数据根目录, 先运行 `wxemo config init`, 或用 --data-root 指定");
    }

    let accounts = discover::find_accounts(&roots)?;
    let account = discover::select_account(
        &accounts,
        options.account.as_deref(),
        config.default_account.as_deref(),
        interactive,
    )?;
    Ok((account.wxid.clone(), account.emoticon_dir()))
}

/// 找出与 wxid 对应的完整目录名 (带后缀那种写法), 用于兜底尝试.
fn folder_name_variant(
    options: &SeedOptions,
    config: &Config,
    wxid: &str,
    interactive: bool,
) -> Option<String> {
    let roots = discover::defaults::resolve(&options.data_roots, &config.data_roots).ok()?;
    let accounts = discover::find_accounts(&roots).ok()?;
    let account = discover::select_account(
        &accounts,
        options.account.as_deref(),
        config.default_account.as_deref(),
        interactive,
    )
    .ok()?;
    if account.folder_name == wxid {
        None
    } else {
        Some(account.folder_name.clone())
    }
}

/// 从密文目录里挑若干样本.
///
/// 优先挑 Persist 目录下体积适中的文件: 太小可能不是完整图片, 太大则读取慢.
fn pick_samples(emoticon_dir: &Path, want: usize) -> Result<Vec<Sample>> {
    let mut candidates: Vec<PathBuf> = Vec::new();
    for sub in ["Persist", "PersistStore"] {
        let dir = emoticon_dir.join(sub);
        if !dir.is_dir() {
            continue;
        }
        collect_files(&dir, &mut candidates);
    }
    if candidates.is_empty() {
        bail!("在 {} 下没有找到密文文件", emoticon_dir.display());
    }

    // 按大小排序, 取中间一段, 避开极端值.
    let mut sized: Vec<(u64, PathBuf)> = candidates
        .into_iter()
        .filter_map(|path| {
            let size = fs::metadata(&path).ok()?.len();
            Some((size, path))
        })
        .filter(|(size, _)| *size >= 64)
        .collect();
    sized.sort();

    let want = want.max(1);
    if sized.len() < want {
        bail!("可用样本只有 {} 个, 少于要求的 {want} 个", sized.len());
    }

    // 在中间区域均匀取样, 尽量来自不同表情.
    let mid = sized.len() / 2;
    let mut picked = Vec::new();
    let mut step = 1usize;
    if sized.len() > want {
        step = sized.len() / want;
    }
    let mut index = mid.saturating_sub(want * step / 2);
    while picked.len() < want && index < sized.len() {
        let (_, path) = &sized[index];
        let body = fs::read(path).with_context(|| format!("读取样本失败: {}", path.display()))?;
        if body.len() >= 32 && body.len() % 16 == 0 {
            picked.push(Sample {
                name: path.display().to_string(),
                prefix: body[..16].to_vec(),
                body,
            });
        }
        index += step.max(1);
    }

    if picked.len() < want {
        bail!("只凑到 {} 个合法样本, 少于要求的 {want} 个", picked.len());
    }
    Ok(picked)
}

/// 递归收集文件.
fn collect_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_files(&path, out);
        } else if path.is_file() {
            out.push(path);
        }
    }
}

/// 供测试使用: 用给定 seed 派生密钥.
#[allow(dead_code)]
pub fn key_for(seed: u64, wxid: &str) -> [u8; 16] {
    let mut message = [0u8; MAX_MESSAGE];
    crate::crypto::derive_key(seed, wxid.as_bytes(), &mut message)
}
