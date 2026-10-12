//! `wxemo accounts` 子命令: 列出发现的账号.

use anyhow::Result;

use crate::config::{Auth, Config};
use crate::discover;

/// 列出数据根目录下发现的所有账号.
pub fn run(roots: &[String]) -> Result<()> {
    let config = Config::load()?.0;
    let resolved: Vec<std::path::PathBuf> = if roots.is_empty() {
        config.data_roots.clone()
    } else {
        roots.iter().map(|raw| discover::normalize_root(raw)).collect()
    };

    if resolved.is_empty() {
        println!("尚未配置数据根目录, 先运行 `wxemo config init`");
        println!("或直接指定: wxemo accounts --data-root D:/wechat");
        return Ok(());
    }

    println!("搜索根目录:");
    for root in &resolved {
        let mark = if root.exists() { "存在" } else { "不存在" };
        println!("  {} ({mark})", root.display());
    }

    let accounts = discover::find_accounts(&resolved)?;
    if accounts.is_empty() {
        println!("\n未发现任何账号");
        return Ok(());
    }

    let auth = Auth::load().unwrap_or_default();
    let default = config.default_account.as_deref();
    println!("\n发现 {} 个账号:", accounts.len());
    for account in &accounts {
        let seed_mark = match auth.get(&account.wxid) {
            Some(secret) => format!("已求 seed = {}", secret.seed),
            None => "缺 seed".to_string(),
        };
        let default_mark = if default == Some(account.wxid.as_str()) {
            "  <- 默认"
        } else {
            ""
        };
        println!("  {}{}", account.wxid, default_mark);
        println!("    目录: {}", account.dir.display());
        println!("    目录名: {}", account.folder_name);
        println!("    状态: {seed_mark}");
    }
    Ok(())
}
