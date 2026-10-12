//! 已保存 seed 的状态, 导入与删除管理.
use std::io::{self, Write};

use anyhow::{bail, Context, Result};
use tracing::info;

use crate::config::{parse_key, Auth};
use crate::{crypto, discover};

/// 仅规范化显式账号, 管理记录不依赖微信目录仍然存在.
fn account_id(raw: &str) -> Result<String> {
    let raw = raw.trim();
    if raw.is_empty()
        || raw
            .chars()
            .any(|ch| ch.is_whitespace() || ch == '/' || ch == '\\')
    {
        bail!("账号标识不能为空, 也不能包含空白或路径分隔符");
    }
    let account = discover::normalize_wxid(raw);
    if account.is_empty() || account.len() > 128 {
        bail!("账号标识必须为 1 到 128 字节");
    }
    Ok(account)
}

/// 默认只显示保存和密钥格式状态, 不输出敏感值.
pub fn status(account: Option<&str>) -> Result<()> {
    let auth = Auth::load()?;
    if let Some(raw) = account {
        let account = account_id(raw)?;
        show_status(&auth, &account);
    } else if auth.accounts.is_empty() {
        println!("尚未保存 seed 记录, 使用 `wxemo seed search` 或 `wxemo seed import`");
    } else {
        println!("已保存账号: {}", auth.accounts.len());
        for account in auth.accounts.keys() {
            show_status(&auth, account);
        }
    }
    Ok(())
}

fn show_status(auth: &Auth, account: &str) {
    match auth.get(account) {
        Some(secret) => {
            let state = if parse_key(&secret.key).is_ok() {
                "已保存"
            } else {
                "密钥格式异常"
            };
            println!("{account}: {state} (seed 和密钥已隐藏)");
        }
        None => println!("{account}: 未保存"),
    }
}

fn import_record(auth: &mut Auth, raw: &str, seed: u64, force: bool) -> Result<String> {
    let account = account_id(raw)?;
    if auth.get(&account).is_some() && !force {
        bail!("{account} 已有记录, 如需覆盖请加 --force");
    }
    let mut message = [0u8; crypto::MAX_MESSAGE];
    let key = crypto::derive_key(seed, account.as_bytes(), &mut message);
    auth.put(&account, seed, &key);
    Ok(account)
}

/// 导入已知 seed, 派生但不验证密钥, 不覆盖其他账号.
pub fn import(raw: &str, seed: u64, force: bool) -> Result<()> {
    let mut auth = Auth::load()?;
    let account = import_record(&mut auth, raw, seed, force)?;
    auth.save().context("保存导入记录失败")?;
    info!(account, "已导入 seed 并保存派生密钥");
    println!("已导入 {account}, 尚未验证微信密文, 请用 `wxemo extract` 检查结果");
    Ok(())
}

fn remove_record(auth: &mut Auth, raw: &str) -> Result<String> {
    let account = account_id(raw)?;
    if auth.accounts.remove(&account).is_none() {
        bail!("{account} 没有已保存的记录");
    }
    Ok(account)
}

/// 确认后仅移除指定账号的记录.
pub fn remove(raw: &str, yes: bool, interactive: bool) -> Result<()> {
    let account = account_id(raw)?;
    let mut auth = Auth::load()?;
    if auth.get(&account).is_none() {
        bail!("{account} 没有已保存的记录");
    }
    if !yes {
        if !interactive {
            bail!("删除记录需要确认, 非交互运行请显式加 --yes");
        }
        eprint!("删除 {account} 保存的 seed 与密钥? [y/N]: ");
        io::stderr().flush()?;
        let mut answer = String::new();
        io::stdin().read_line(&mut answer)?;
        if !matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes") {
            println!("已取消, 记录未修改");
            return Ok(());
        }
    }
    remove_record(&mut auth, &account)?;
    auth.save().context("保存删除后的记录失败")?;
    info!(account, "已删除保存的 seed 与密钥");
    println!("已删除 {account} 的记录, 微信数据未修改");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn import_normalizes_account_and_requires_explicit_overwrite() {
        let mut auth = Auth::default();
        let account = import_record(&mut auth, "wxid_example_7e1e", 42, false).unwrap();
        assert_eq!(account, "wxid_example");
        let mut message = [0u8; crypto::MAX_MESSAGE];
        let key = crypto::derive_key(42, b"wxid_example", &mut message);
        assert_eq!(parse_key(&auth.get(&account).unwrap().key).unwrap(), key);
        assert!(import_record(&mut auth, &account, 43, false).is_err());
        assert_eq!(auth.get(&account).unwrap().seed, 42);
        import_record(&mut auth, &account, 43, true).unwrap();
        assert_eq!(auth.get(&account).unwrap().seed, 43);
        assert!(import_record(&mut auth, &"a".repeat(129), 42, false).is_err());
        assert!(import_record(&mut auth, " ", 42, false).is_err());
    }

    #[test]
    fn removing_one_account_preserves_others_and_rejects_missing() {
        let mut auth = Auth::default();
        import_record(&mut auth, "wxid_a", 42, false).unwrap();
        import_record(&mut auth, "wxid_b", 43, false).unwrap();
        remove_record(&mut auth, "wxid_a_7e1e").unwrap();
        assert!(auth.get("wxid_a").is_none());
        assert_eq!(auth.get("wxid_b").unwrap().seed, 43);
        assert!(remove_record(&mut auth, "wxid_a").is_err());
        assert_eq!(auth.accounts.len(), 1);
    }
}
