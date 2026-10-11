//! 微信表情包 seed 暴力搜索.
//!
//! 派生公式来自对 Weixin.dll 的静态分析:
//!
//! ```text
//! key = MD5(f"{seed}{账号标识}EMOTICON").digest()[:16]
//! ```

mod derive;
mod logging;
mod search;

use std::fs;
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Instant;

use clap::Parser;
use indicatif::{ProgressBar, ProgressStyle};
use tracing::{error, info, warn};

use crate::search::{run, Sample, SearchSpec};

/// 命令行参数.
#[derive(Parser, Debug)]
#[command(name = "seedsearch", about = "暴力搜索微信表情包派生密钥的 seed")]
struct Args {
    /// 用于校验的密文文件, 可重复指定, 越多误报越少.
    #[arg(long = "sample", required = true)]
    samples: Vec<PathBuf>,

    /// 账号标识候选 (如 wxid_example123456), 可重复指定.
    #[arg(long = "wxid", required = true)]
    wxids: Vec<String>,

    /// 搜索起始值 (含).
    #[arg(long, default_value_t = 0)]
    start: u64,

    /// 搜索结束值 (不含).
    #[arg(long, default_value_t = 1u64 << 32)]
    end: u64,

    /// 工作线程数, 默认取全部核心.
    #[arg(long)]
    threads: Option<usize>,

    /// 命中后写入的结果文件.
    #[arg(long, default_value = "seed-result.txt")]
    out: PathBuf,

    /// 输出 debug 级别日志.
    #[arg(short, long)]
    verbose: bool,
}

fn main() -> ExitCode {
    let args = Args::parse();
    logging::init(args.verbose);

    if args.start >= args.end {
        error!(start = args.start, end = args.end, "搜索区间为空");
        return ExitCode::FAILURE;
    }

    let mut samples = Vec::new();
    for path in &args.samples {
        match fs::read(path) {
            Ok(body) => {
                if body.len() < 32 || body.len() % 16 != 0 {
                    error!(
                        path = %path.display(),
                        len = body.len(),
                        "样本长度不足两个分组或不是 16 的整数倍"
                    );
                    return ExitCode::FAILURE;
                }
                let len = body.len();
                info!(path = %path.display(), len, "载入样本");
                let mut tail_last = [0u8; 16];
                let mut tail_prev = [0u8; 16];
                tail_last.copy_from_slice(&body[len - 16..]);
                tail_prev.copy_from_slice(&body[len - 32..len - 16]);
                samples.push(Sample {
                    name: path.display().to_string(),
                    prefix: body[..16].to_vec(),
                    tail_last,
                    tail_prev,
                    body,
                });
            }
            Err(err) => {
                error!(path = %path.display(), error = %err, "读取样本失败");
                return ExitCode::FAILURE;
            }
        }
    }

    let wxids: Vec<Vec<u8>> = args.wxids.iter().map(|w| w.as_bytes().to_vec()).collect();
    let total = args.end - args.start;
    info!(
        start = args.start,
        end = args.end,
        candidates = total,
        wxids = ?args.wxids,
        sample_count = samples.len(),
        "开始搜索"
    );

    if let Some(threads) = args.threads {
        if let Err(err) = rayon::ThreadPoolBuilder::new().num_threads(threads).build_global() {
            warn!(error = %err, "线程池设置失败, 使用默认值");
        }
    }

    let bar = ProgressBar::new(total);
    bar.set_style(
        ProgressStyle::with_template(
            "{spinner:.green} {percent:>5.1}%  {per_sec:>12}  已用 {elapsed:>8}  剩余 {eta:>8}  [{bar:28}]",
        )
        .expect("进度条模板非法")
        .progress_chars("=> "),
    );

    let spec = SearchSpec {
        start: args.start,
        end: args.end,
        wxids,
        samples,
    };

    let started = Instant::now();
    let hit = run(&spec, &bar);
    bar.finish_and_clear();

    let elapsed = started.elapsed();
    match hit {
        Some(found) => {
            let rate = total as f64 / elapsed.as_secs_f64();
            info!(
                seed = found.seed,
                wxid = %String::from_utf8_lossy(&found.wxid),
                key = %hex(&found.key),
                format = %found.format,
                seconds = elapsed.as_secs_f64(),
                rate = rate as u64,
                "命中"
            );
            println!("seed  = {}", found.seed);
            println!("wxid  = {}", String::from_utf8_lossy(&found.wxid));
            println!("key   = {}", hex(&found.key));
            println!("格式  = {}", found.format);
            println!("填充合法 = {}", found.padding_ok);
            println!("命中样本 = {}", found.matched.join(", "));
            let report = format!(
                "seed={}\nwxid={}\nkey={}\nformat={}\npadding_ok={}\n",
                found.seed,
                String::from_utf8_lossy(&found.wxid),
                hex(&found.key),
                found.format,
                found.padding_ok
            );
            if let Err(err) = fs::write(&args.out, report) {
                error!(path = %args.out.display(), error = %err, "写入结果失败");
                return ExitCode::FAILURE;
            }
            info!(path = %args.out.display(), "结果已写入");
            ExitCode::SUCCESS
        }
        None => {
            error!(seconds = elapsed.as_secs_f64(), "区间内未找到 seed");
            ExitCode::FAILURE
        }
    }
}

/// 十六进制小写表示.
fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}
