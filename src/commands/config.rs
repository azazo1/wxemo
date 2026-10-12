//! `wxemo config` 子命令: 初始化与查看配置.

use std::io::Write;

use anyhow::{Context, Result};

use crate::config::{auth_path, config_dir, config_path, Auth, Config};
use crate::discover;

/// `wxemo config init`.
///
/// 优先使用显式路径, 否则探测系统默认位置; 只有缺少路径或有多个候选时询问.
pub fn init(data_root: Option<String>, force: bool, interactive: bool) -> Result<()> {
    let path = config_path();
    if path.exists() && !force {
        println!("配置已存在: {}", path.display());
        println!("如需重建配置, 加 --force");
        return Ok(());
    }

    let root = match data_root {
        Some(raw) => {
            anyhow::ensure!(!raw.trim().is_empty(), "数据根目录不能为空");
            discover::normalize_root(&raw)
        }
        None => {
            let roots = discover::defaults::resolve(&[], &[])?;
            if roots.len() == 1 {
                println!("自动采用微信数据目录: {}", roots[0].display());
                roots[0].clone()
            } else {
                if !interactive {
                    anyhow::bail!(
                        "系统默认位置找到 {} 个有效数据目录, 请用 --data-root 明确指定",
                        roots.len()
                    );
                }
                if !roots.is_empty() {
                    eprintln!("发现多个数据目录, 请选择:");
                    for (index, root) in roots.iter().enumerate() {
                        eprintln!("  [{}] {}", index + 1, root.display());
                    }
                    eprint!("输入序号 (1-{}): ", roots.len());
                } else {
                    eprint!("未找到支持的默认目录, 输入微信数据根目录: ");
                }
                std::io::stderr().flush()?;
                let mut line = String::new();
                std::io::stdin().read_line(&mut line)?;
                if roots.is_empty() {
                    anyhow::ensure!(!line.trim().is_empty(), "数据根目录不能为空");
                    discover::normalize_root(line.trim())
                } else {
                    let choice: usize = line.trim().parse().context("无法解析选择序号")?;
                    roots
                        .get(choice.wrapping_sub(1))
                        .context("目录序号超出范围")?
                        .clone()
                }
            }
        }
    };
    let config = Config {
        data_roots: vec![root.clone()],
        ..Config::default()
    };
    config.save()?;

    println!("已写入配置: {}", path.display());
    println!("  数据根目录: {}", root.display());
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
    println!(
        "配置文件: {} ({})",
        config_path().display(),
        if initialized { "已就绪" } else { "缺失" }
    );
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
