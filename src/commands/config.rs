//! `wxemo config` 子命令: 初始化与查看配置.

use std::io::{IsTerminal, Write};

use anyhow::{Context, Result};

use crate::config::{config_dir, config_path, auth_path, Auth, Config};
use crate::discover;

/// `wxemo config init`.
///
/// 交互式地问数据根目录, 然后写入配置. 非交互环境下必须用 `--data-root`.
pub fn init(data_root: Option<String>, force: bool) -> Result<()> {
    let path = config_path();
    if path.exists() && !force {
        println!("配置已存在: {}", path.display());
        println!("如需重建配置, 加 --force");
        return Ok(());
    }

    let raw = match data_root {
        Some(value) => value,
        None => {
            if !std::io::stdin().is_terminal() {
                anyhow::bail!(
                    "标准输入不是终端, 请用 --data-root 指定微信数据根目录, 例如:\n  \
                     wxemo config init --data-root D:/wechat"
                );
            }
            print!("微信数据根目录 (例如 D:/wechat 或 ~/xwechat_files): ");
            std::io::stdout().flush()?;
            let mut line = String::new();
            std::io::stdin().read_line(&mut line)?;
            line.trim().to_string()
        }
    };

    if raw.is_empty() {
        anyhow::bail!("数据根目录不能为空");
    }

    let root = discover::normalize_root(&raw);
    let config = Config { data_roots: vec![root.clone()], ..Config::default() };
    config.save()?;

    println!("已写入配置: {}", path.display());
    println!("  数据根目录: {} ({})", raw, root.display());
    if !root.exists() {
        println!("  提示: 该目录当前不存在, 请确认路径是否正确");
    }

    // 顺手探测一次账号, 让用户马上知道配对没有.
    match discover::find_accounts(&config.data_roots) {
        Ok(accounts) if accounts.is_empty() => {
            println!("  尚未发现任何账号, 请确认微信已登录过且目录正确");
        }
        Ok(accounts) => {
            println!("  发现 {} 个账号:", accounts.len());
            for account in accounts {
                println!("    {}  ({})", account.wxid, account.dir.display());
            }
        }
        Err(err) => println!("  账号探测失败: {err:#}"),
    }
    Ok(())
}

/// `wxemo config status`.
pub fn status() -> Result<()> {
    let dir = config_dir();
    println!("配置目录: {}", dir.display());
    if !dir.exists() {
        println!("  状态: 尚未初始化, 先运行 `wxemo config init`");
        return Ok(());
    }

    let (config, initialized) = Config::load()?;
    println!("配置文件: {} ({})", config_path().display(), if initialized { "已就绪" } else { "缺失" });
    println!("  格式版本: {}", config.version);
    if config.data_roots.is_empty() {
        println!("  数据根目录: 未配置");
    } else {
        println!("  数据根目录:");
        for root in &config.data_roots {
            let mark = if root.exists() { "存在" } else { "不存在" };
            println!("    {} ({mark})", root.display());
        }
    }
    println!(
        "  默认账号: {}",
        config.default_account.as_deref().unwrap_or("(未设置)")
    );
    println!(
        "  搜索范围: {} .. {}   线程: {}",
        config.search.start,
        config.search.end,
        if config.search.threads == 0 {
            "全部核心".to_string()
        } else {
            config.search.threads.to_string()
        }
    );

    let auth = Auth::load()?;
    let auth_exist = auth_path().exists();
    println!(
        "密钥文件: {} ({})",
        auth_path().display(),
        if auth_exist { "已就绪" } else { "缺失" }
    );
    if auth.accounts.is_empty() {
        println!("  已记录账号: 无");
    } else {
        println!("  已记录账号:");
        for (wxid, secret) in &auth.accounts {
            println!(
                "    {}  seed={}  key={}  ({})",
                wxid, secret.seed, secret.key, secret.updated_at
            );
        }
    }

    let accounts = discover::find_accounts(&config.data_roots)?;
    println!("磁盘上发现的账号: {}", accounts.len());
    for account in &accounts {
        let known = if auth.get(&account.wxid).is_some() {
            "已求 seed"
        } else {
            "缺 seed"
        };
        println!("    {}  [{known}]  {}", account.wxid, account.dir.display());
    }

    if let Some(default) = &config.default_account {
        if !accounts.iter().any(|account| &account.wxid == default) {
            println!("  警告: 默认账号 {default} 在磁盘上不存在");
        }
    }
    Ok(())
}

/// `wxemo config path`.
pub fn path() -> Result<()> {
    println!("{}", config_dir().display());
    Ok(())
}

/// `wxemo config set-root <路径>`: 替换数据根目录列表.
pub fn set_root(value: &str) -> Result<()> {
    let (mut config, _) = Config::load()?;
    let root = discover::normalize_root(value);
    if !config.data_roots.contains(&root) {
        config.data_roots.push(root.clone());
    }
    config.save().context("写入配置失败")?;
    println!("已添加数据根目录: {} ({})", value, root.display());
    Ok(())
}
