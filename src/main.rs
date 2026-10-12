//! wxemo: 微信 4.x 自定义表情包的 seed 搜索与解密工具.
//!
//! 派生公式来自对 Weixin.dll 的静态分析:
//!
//! ```text
//! key       = MD5(f"{seed}{wxid}EMOTICON").digest()[:16]
//! plaintext = AES-128-CBC(key, iv=key).decrypt(ciphertext)   # PKCS#7 填充
//! ```

mod commands;
mod config;
mod discover;
mod emoticon;
mod logging;
mod version;

use emoticon::{container, crypto, search};

use std::io::IsTerminal;
use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Args, Parser, Subcommand};

use commands::extract::ExtractOptions;
use commands::seed::SeedOptions;

/// 微信表情包工具.
#[derive(Parser, Debug)]
#[command(name = "wxemo", version = version::version(), about = "微信 4.x 自定义表情包的 seed 搜索与解密工具")]
struct Cli {
    /// 输出 debug 级别日志.
    #[arg(short, long, global = true)]
    verbose: bool,

    /// 禁用交互式询问 (多账号时直接报错要求 --account).
    #[arg(long, global = true)]
    no_interactive: bool,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// 管理配置与账号密钥.
    #[command(subcommand)]
    Config(ConfigCommand),

    /// 列出发现的微信账号.
    Accounts(AccountsArgs),

    /// 搜索, 查看, 导入或删除账号的 seed 记录.
    #[command(subcommand)]
    Seed(SeedCommand),

    /// 用已知密钥解密并还原表情包.
    Extract(ExtractArgs),

    /// 检查环境与配置是否就绪.
    Doctor(DoctorArgs),
}

#[derive(Subcommand, Debug)]
enum ConfigCommand {
    /// 交互式初始化配置.
    Init {
        /// 微信数据根目录, 省略时自动发现系统默认位置.
        #[arg(long)]
        data_root: Option<String>,
        /// 覆盖已存在的配置.
        #[arg(long)]
        force: bool,
    },
    /// 查看配置与密钥状态.
    Status,
    /// 打印配置目录路径.
    Path,
    /// 追加一个数据根目录.
    SetRoot {
        /// 根目录路径, 例如 D:/wechat.
        path: String,
    },
}

#[derive(Args, Debug)]
struct AccountsArgs {
    /// 额外/替代的数据根目录, 可重复.
    #[arg(long = "data-root")]
    data_roots: Vec<String>,
}

#[derive(Subcommand, Debug)]
enum SeedCommand {
    /// 暴力搜索 seed, 并保存派生出的密钥.
    Search(SeedArgs),
    /// 查看已保存记录的状态, 不显示 seed 或密钥.
    Status {
        /// 仅查看指定账号, 省略时列出全部已保存记录.
        #[arg(long)]
        account: Option<String>,
    },
    /// 导入已知 seed 并派生密钥, 不搜索也不验证微信密文.
    Import {
        /// 已知的十进制 seed.
        seed: u64,
        /// 账号标识 (wxid) 或账号目录名.
        #[arg(long)]
        account: String,
        /// 覆盖该账号已有的记录.
        #[arg(long)]
        force: bool,
    },
    /// 删除本工具保存的 seed 与密钥, 不修改微信数据.
    Remove {
        /// 要删除记录的账号标识或目录名.
        #[arg(long)]
        account: String,
        /// 跳过确认, 非交互运行必须提供.
        #[arg(long)]
        yes: bool,
    },
}

#[derive(Args, Debug)]
struct SeedArgs {
    /// 账号标识 (wxid) 或账号目录名.
    #[arg(long)]
    account: Option<String>,

    /// 直接指定表情包密文目录, 跳过账号发现.
    #[arg(long)]
    emoticon_dir: Option<PathBuf>,

    /// 额外/替代的数据根目录, 可重复.
    #[arg(long = "data-root")]
    data_roots: Vec<String>,

    /// 搜索起点 (含), 默认取配置.
    #[arg(long)]
    start: Option<u64>,

    /// 搜索终点 (不含), 默认取配置.
    #[arg(long)]
    end: Option<u64>,

    /// 线程数, 默认取配置 (0 表示全部核心).
    #[arg(long)]
    threads: Option<usize>,

    /// 判定用的样本数量.
    #[arg(long, default_value_t = 3)]
    samples: usize,
}

#[derive(Args, Debug)]
struct ExtractArgs {
    /// 账号标识 (wxid) 或账号目录名.
    #[arg(long)]
    account: Option<String>,

    /// 直接指定表情包密文目录.
    #[arg(long)]
    emoticon_dir: Option<PathBuf>,

    /// 输出目录.
    #[arg(long, short, default_value = "out/emoticons")]
    out: PathBuf,

    /// 直接指定 32 位十六进制密钥, 优先于配置.
    #[arg(long)]
    key: Option<String>,

    /// 直接指定 seed, 与账号标识一起派生密钥.
    #[arg(long)]
    seed: Option<u64>,

    /// 额外/替代的数据根目录, 可重复.
    #[arg(long = "data-root")]
    data_roots: Vec<String>,

    /// 多帧容器不拆分, 整段输出.
    #[arg(long)]
    no_split: bool,

    /// 在输出目录写入 manifest.tsv 清单.
    #[arg(long)]
    manifest: bool,
}

#[derive(Args, Debug)]
struct DoctorArgs {
    /// 额外/替代的数据根目录, 可重复.
    #[arg(long = "data-root")]
    data_roots: Vec<String>,
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    logging::init(cli.verbose);

    // 交互式的前提: 允许交互, 且标准输入是终端.
    let interactive = !cli.no_interactive && std::io::stdin().is_terminal();

    let result = match cli.command {
        Command::Config(command) => match command {
            ConfigCommand::Init { data_root, force } => {
                commands::config::init(data_root, force, interactive)
            }
            ConfigCommand::Status => commands::config::status(),
            ConfigCommand::Path => commands::config::path(),
            ConfigCommand::SetRoot { path } => commands::config::set_root(&path),
        },
        Command::Accounts(args) => commands::accounts::run(&args.data_roots),
        Command::Seed(command) => match command {
            SeedCommand::Search(args) => commands::seed::run_seed(
                SeedOptions {
                    account: args.account,
                    emoticon_dir: args.emoticon_dir,
                    data_roots: args.data_roots,
                    start: args.start,
                    end: args.end,
                    threads: args.threads,
                    sample_count: args.samples,
                },
                interactive,
            ),
            SeedCommand::Status { account } => commands::seed::manage::status(account.as_deref()),
            SeedCommand::Import {
                seed,
                account,
                force,
            } => commands::seed::manage::import(&account, seed, force),
            SeedCommand::Remove { account, yes } => {
                commands::seed::manage::remove(&account, yes, interactive)
            }
        },
        Command::Extract(args) => commands::extract::run_extract(
            ExtractOptions {
                account: args.account,
                emoticon_dir: args.emoticon_dir,
                out: args.out,
                key: args.key,
                seed: args.seed,
                data_roots: args.data_roots,
                split_frames: !args.no_split,
                manifest: args.manifest,
            },
            interactive,
        ),
        Command::Doctor(args) => commands::doctor::run(&args.data_roots),
    };

    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("错误: {err:#}");
            ExitCode::FAILURE
        }
    }
}
