//! `wxemo doctor` 环境诊断.
use crate::{config, discover};

pub fn run(data_roots: &[String]) -> anyhow::Result<()> {
    let dir = config::config_dir();
    println!("配置目录: {}", dir.display());
    println!(
        "目录状态: {}",
        if dir.is_dir() {
            "已存在"
        } else {
            "尚未创建"
        }
    );
    let (cfg, initialized) = config::Config::load()?;
    println!(
        "配置: {}",
        if initialized {
            "已加载"
        } else {
            "使用默认值"
        }
    );
    let roots = discover::defaults::resolve(data_roots, &cfg.data_roots)?;
    if roots.is_empty() {
        println!("数据根目录: 未配置");
    } else {
        println!("数据根目录:");
        for root in &roots {
            println!(
                "  {} ({})",
                root.display(),
                if root.exists() { "存在" } else { "不存在" }
            );
        }
    }
    let accounts = discover::find_accounts(&roots)?;
    println!("账号: {}", accounts.len());
    let auth = config::Auth::load()?;
    for account in &accounts {
        let state = if auth.get(&account.wxid).is_some() {
            "已保存密钥"
        } else {
            "缺 seed, 运行 `wxemo seed search`"
        };
        println!("  {}  [{}]", account.wxid, state);
    }
    println!("线程: {}", rayon::current_num_threads());
    Ok(())
}
