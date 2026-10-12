//! 微信数据根目录与账号的发现.
//!
//! 微信 4.x 的磁盘布局通常是:
//!
//! ```text
//! <根目录>/.../xwechat_files/<账号目录>/business/emoticon/{Persist,PersistStore,...}
//! ```
//!
//! 根目录由用户配置 (例如 `D:/wechat`), 账号目录名形如
//! `wxid_xxxxxxxx_7e1e` —— 尾部 4 位十六进制是微信加的, 不参与密钥派生.

mod access;
pub mod defaults;

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};

/// 存放账号的目录名.
const ACCOUNT_ROOT: &str = "xwechat_files";

/// 从根目录向下搜索的最大层数.
const MAX_DEPTH: usize = 5;

/// 一个微信账号.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Account {
    /// 账号目录名, 例如 `wxid_xxxxxxxx_7e1e`.
    pub folder_name: String,
    /// 参与密钥派生的账号标识, 即去掉尾部 `_xxxx` 之后的目录名.
    pub wxid: String,
    /// 账号目录绝对路径.
    pub dir: PathBuf,
}

impl Account {
    /// 表情包密文目录.
    pub fn emoticon_dir(&self) -> PathBuf {
        self.dir.join("business").join("emoticon")
    }
}

/// 去掉账号目录名尾部的 `_xxxx` 后缀, 得到参与派生的 wxid.
pub fn normalize_wxid(folder_name: &str) -> String {
    let Some((head, tail)) = folder_name.rsplit_once('_') else {
        return folder_name.to_string();
    };
    if tail.len() == 4 && tail.chars().all(|c| c.is_ascii_hexdigit()) {
        head.to_string()
    } else {
        folder_name.to_string()
    }
}

/// 把用户写的根目录规范化.
///
/// 在 WSL 里用户很可能直接写 `D:/wechat` 或 `D:\wechat`, 需要映射到 `/mnt/d/wechat`.
pub fn normalize_root(raw: &str) -> PathBuf {
    let trimmed = raw.trim().trim_matches('"');
    if cfg!(target_os = "linux") {
        let bytes = trimmed.as_bytes();
        if bytes.len() >= 2 && bytes[1] == b':' && bytes[0].is_ascii_alphabetic() {
            let drive = (bytes[0] as char).to_ascii_lowercase();
            let rest = trimmed[2..].trim_start_matches(['/', '\\']);
            return PathBuf::from(format!("/mnt/{drive}/{rest}"));
        }
    }
    let expanded = if let Some(rest) = trimmed.strip_prefix("~/") {
        match std::env::var_os("HOME") {
            Some(home) => PathBuf::from(home).join(rest),
            None => PathBuf::from(trimmed),
        }
    } else {
        PathBuf::from(trimmed)
    };
    expanded
}

/// 在根目录下递归找 `xwechat_files`, 返回找到的目录列表.
fn find_account_roots(root: &Path) -> Result<Vec<PathBuf>> {
    let mut found = Vec::new();
    let mut queue = vec![(root.to_path_buf(), 0usize)];
    while let Some((dir, depth)) = queue.pop() {
        if dir.file_name().is_some_and(|name| name == ACCOUNT_ROOT) {
            found.push(dir);
            continue;
        }
        if depth >= MAX_DEPTH {
            continue;
        }
        let Some(entries) = access::read_directory(&dir)? else {
            continue;
        };
        for entry in entries {
            let entry = entry.with_context(|| format!("读取目录项失败: {}", dir.display()))?;
            let path = entry.path();
            // 不跟随符号链接, 避免在数据目录中绕圈.
            let kind = entry.file_type().with_context(|| format!("读取文件类型失败: {}", path.display()))?;
            if kind.is_dir() {
                queue.push((path, depth + 1));
            }
        }
    }
    found.sort();
    Ok(found)
}

/// 判断目录是否像一个账号目录.
fn looks_like_account(dir: &Path) -> Result<bool> {
    access::is_directory(&dir.join("business").join("emoticon"))
}

/// 从若干根目录中发现全部账号, 按 wxid 去重.
pub fn find_accounts(roots: &[PathBuf]) -> Result<Vec<Account>> {
    let mut accounts: Vec<Account> = Vec::new();
    let mut seen: Vec<PathBuf> = Vec::new();

    for root in roots {
        let Some(entries) = access::read_directory(root)? else {
            tracing::warn!(root = %root.display(), "数据根目录不存在, 已跳过");
            continue;
        };
        drop(entries);

        // 情形一: 根目录本身就是账号目录.
        let mut candidates = Vec::new();
        if looks_like_account(root)? {
            candidates.push(root.clone());
        }

        // 情形二: 根目录下有 xwechat_files.
        for account_root in find_account_roots(root)? {
            let Some(entries) = access::read_directory(&account_root)? else {
                continue;
            };
            for entry in entries {
                let entry = entry.with_context(|| format!("读取目录项失败: {}", account_root.display()))?;
                let path = entry.path();
                let kind = entry.file_type().with_context(|| format!("读取文件类型失败: {}", path.display()))?;
                if kind.is_dir() && looks_like_account(&path)? {
                    candidates.push(path);
                }
            }
        }

        for dir in candidates {
            let canonical = dir.canonicalize().unwrap_or_else(|_| dir.clone());
            if seen.contains(&canonical) {
                continue;
            }
            seen.push(canonical);
            let folder_name = dir
                .file_name()
                .map(|name| name.to_string_lossy().to_string())
                .unwrap_or_default();
            accounts.push(Account {
                wxid: normalize_wxid(&folder_name),
                folder_name,
                dir,
            });
        }
    }

    accounts.sort_by(|a, b| a.wxid.cmp(&b.wxid));
    Ok(accounts)
}

/// 选定要操作的账号.
///
/// - 显式给了 `--account` 就按 wxid 或目录名匹配;
/// - 否则用配置里的默认账号;
/// - 再否则, 只有一个账号就直接用, 多个账号时: 有终端就交互式询问,
///   没有终端 (管道/重定向) 就报错要求显式指定.
pub fn select_account<'a>(
    accounts: &'a [Account],
    explicit: Option<&str>,
    default_account: Option<&str>,
    interactive: bool,
) -> Result<&'a Account> {
    if accounts.is_empty() {
        bail!("没有发现任何账号, 请用 `wxemo config init` 配置数据根目录");
    }

    let find = |needle: &str| {
        accounts
            .iter()
            .find(|account| account.wxid == needle || account.folder_name == needle)
    };

    if let Some(needle) = explicit {
        return find(needle).with_context(|| {
            let known: Vec<&str> = accounts.iter().map(|a| a.wxid.as_str()).collect();
            format!("找不到账号 {needle}, 已知账号: {}", known.join(", "))
        });
    }

    if let Some(needle) = default_account {
        if let Some(account) = find(needle) {
            return Ok(account);
        }
        tracing::warn!(account = needle, "配置的默认账号不存在, 改为自动选择");
    }

    if accounts.len() == 1 {
        return Ok(&accounts[0]);
    }

    if !interactive {
        let known: Vec<&str> = accounts.iter().map(|a| a.wxid.as_str()).collect();
        bail!(
            "发现多个账号, 当前不是交互式终端, 请用 --account 指定其中一个: {}",
            known.join(", ")
        );
    }

    prompt_account(accounts)
}

/// 在终端里让用户从多个账号中挑一个.
fn prompt_account(accounts: &[Account]) -> Result<&Account> {
    use std::io::{BufRead, IsTerminal, Write};

    if !std::io::stdin().is_terminal() {
        let known: Vec<&str> = accounts.iter().map(|a| a.wxid.as_str()).collect();
        bail!(
            "发现多个账号, 标准输入不是终端, 请用 --account 指定: {}",
            known.join(", ")
        );
    }

    let mut stderr = std::io::stderr();
    writeln!(stderr, "发现多个微信账号, 请选择:")?;
    for (index, account) in accounts.iter().enumerate() {
        writeln!(
            stderr,
            "  [{}] {}  ({})",
            index + 1,
            account.wxid,
            account.dir.display()
        )?;
    }
    write!(stderr, "输入序号 (1-{}): ", accounts.len())?;
    stderr.flush()?;

    let mut line = String::new();
    std::io::stdin().lock().read_line(&mut line)?;
    let choice: usize = line
        .trim()
        .parse()
        .with_context(|| format!("无法解析选择: {}", line.trim()))?;
    if choice < 1 || choice > accounts.len() {
        bail!("序号超出范围: {choice}");
    }
    Ok(&accounts[choice - 1])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wxid_suffix_is_stripped() {
        assert_eq!(
            normalize_wxid("wxid_example123456_7e1e"),
            "wxid_example123456"
        );
        assert_eq!(normalize_wxid("wxid_example123456"), "wxid_example123456");
        // 尾部不是 4 位十六进制时保持原样
        assert_eq!(normalize_wxid("wxid_example_zzzz"), "wxid_example_zzzz");
        assert_eq!(normalize_wxid("wxid_example_7e1e2"), "wxid_example_7e1e2");
        assert_eq!(normalize_wxid("plain"), "plain");
    }

    #[test]
    fn windows_drive_maps_to_wsl_mount() {
        if cfg!(target_os = "linux") {
            assert_eq!(normalize_root("D:/wechat"), PathBuf::from("/mnt/d/wechat"));
            assert_eq!(normalize_root("D:\\wechat"), PathBuf::from("/mnt/d/wechat"));
            assert_eq!(normalize_root("C:/"), PathBuf::from("/mnt/c/"));
        }
    }

    #[test]
    fn select_prefers_explicit_then_default_then_single() {
        let accounts = vec![
            Account {
                folder_name: "wxid_a_7e1e".into(),
                wxid: "wxid_a".into(),
                dir: PathBuf::from("/tmp/a"),
            },
            Account {
                folder_name: "wxid_b_7e1e".into(),
                wxid: "wxid_b".into(),
                dir: PathBuf::from("/tmp/b"),
            },
        ];

        let picked = select_account(&accounts, Some("wxid_b"), None, false).unwrap();
        assert_eq!(picked.wxid, "wxid_b");
        // 目录名也能匹配
        let picked = select_account(&accounts, Some("wxid_a_7e1e"), None, false).unwrap();
        assert_eq!(picked.wxid, "wxid_a");
        let picked = select_account(&accounts, None, Some("wxid_b"), false).unwrap();
        assert_eq!(picked.wxid, "wxid_b");

        // 多个账号且非交互时必须报错
        assert!(select_account(&accounts, None, None, false).is_err());

        // 单个账号可以直接选
        let single = vec![accounts[0].clone()];
        assert_eq!(
            select_account(&single, None, None, false).unwrap().wxid,
            "wxid_a"
        );
    }
}
