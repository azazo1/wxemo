//! seed 备份导出与恢复, 不在日志或终端输出敏感值.
use std::collections::BTreeMap;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{ensure, Context, Result};
use serde::{Deserialize, Serialize};
use tracing::info;

use super::manage::{account_id, import_record};
use crate::config::{auth_path, config_path, parse_key, Auth};
use crate::crypto;

const BACKUP_VERSION: u32 = 1;

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Backup {
    version: u32,
    accounts: BTreeMap<String, SeedRecord>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SeedRecord {
    seed: u64,
}

fn backup(auth: &Auth, selected: Option<&str>) -> Result<Backup> {
    let selected = selected.map(account_id).transpose()?;
    if let Some(account) = &selected {
        ensure!(auth.get(account).is_some(), "{account} 没有已保存的记录");
    }
    let mut accounts = BTreeMap::new();
    for (raw, secret) in &auth.accounts {
        if selected.as_ref().is_some_and(|account| account != raw) { continue; }
        let account = account_id(raw)?;
        let mut message = [0u8; crypto::MAX_MESSAGE];
        let expected = crypto::derive_key(secret.seed, account.as_bytes(), &mut message);
        ensure!(parse_key(&secret.key)? == expected, "{account} 的保存密钥与 seed 派生结果不一致, 无法生成可恢复备份");
        ensure!(accounts.insert(account, SeedRecord { seed: secret.seed }).is_none(), "账号规范化后重复, 无法导出");
    }
    ensure!(!accounts.is_empty(), "没有可导出的 seed 记录");
    Ok(Backup { version: BACKUP_VERSION, accounts })
}

/// 比较现有文件的真实路径, 也处理尚不存在的目标文件和父目录别名.
fn resolved_file(path: &Path) -> Result<PathBuf> {
    match path.canonicalize() {
        Ok(path) => Ok(path),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            let parent = path.parent().filter(|path| !path.as_os_str().is_empty()).unwrap_or(Path::new("."));
            let name = path.file_name().context("输出路径必须包含文件名")?;
            Ok(parent.canonicalize()?.join(name))
        }
        Err(err) => Err(err).with_context(|| format!("检查输出路径失败: {}", path.display())),
    }
}

fn create_private(path: &Path) -> Result<fs::File> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path).with_context(|| format!("创建备份失败: {}, 文件已存在时需加 --force", path.display()))
}

fn write_backup(out: &Path, bytes: &[u8], force: bool) -> Result<()> {
    let parent = out.parent().filter(|path| !path.as_os_str().is_empty()).unwrap_or(Path::new("."));
    fs::create_dir_all(parent).with_context(|| format!("创建备份目录失败: {}", parent.display()))?;
    let resolved = resolved_file(out)?;
    for protected in [auth_path(), config_path()] {
        // 活动配置目录未创建时, 文件不可能与已有输出位置重合.
        if let Some(parent) = protected.parent() {
            if parent.is_dir() {
                ensure!(resolved != resolved_file(&protected)?, "不能将备份写到正在使用的配置或密钥文件");
            }
        }
    }
    if !force {
        let mut file = create_private(out)?;
        file.write_all(bytes).context("写入备份失败, 新输出文件可能不完整")?;
        file.sync_all().context("同步备份失败")?;
        return Ok(());
    }
    match fs::symlink_metadata(out) {
        Ok(metadata) => ensure!(metadata.file_type().is_file(), "--force 只允许替换普通文件, 不覆盖符号链接或目录"),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {},
        Err(err) => return Err(err).context("检查备份目标失败"),
    }
    // 覆盖时先完成新文件, 再替换目标, 避免写入失败截断旧备份.
    let nonce = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
    let temporary = parent.join(format!(".wxemo-seed-export-{}-{nonce}.tmp", std::process::id()));
    let mut file = create_private(&temporary)?;
    let result = (|| -> Result<()> {
        file.write_all(bytes).context("写入临时备份失败")?;
        file.sync_all().context("同步临时备份失败")?;
        drop(file);
        fs::rename(&temporary, out).context("替换备份文件失败")?;
        Ok(())
    })();
    if result.is_err() {
        // 临时路径由本函数创建且未替换为其他用途, 只清理该文件.
        let _ = fs::remove_file(&temporary);
    }
    result
}

/// JSON 备份只包含账号和 seed, key 在恢复时重新派生.
pub fn export(account: Option<&str>, out: &Path, force: bool) -> Result<()> {
    let data = backup(&Auth::load()?, account)?;
    let mut bytes = serde_json::to_vec_pretty(&data).context("序列化 seed 备份失败")?;
    bytes.push(b'\n');
    write_backup(out, &bytes, force)?;
    info!(accounts = data.accounts.len(), path = %out.display(), "已导出 seed 备份");
    println!("已导出 {} 个账号到 {}, 文件含敏感 seed, 请妥善保管", data.accounts.len(), out.display());
    Ok(())
}

fn restore(auth: &Auth, data: Backup, selected: Option<&str>, force: bool) -> Result<(Auth, usize)> {
    ensure!(data.version == BACKUP_VERSION, "不支持的 seed 备份格式版本: {}", data.version);
    ensure!(!data.accounts.is_empty(), "备份中没有 seed 记录");
    let selected = selected.map(account_id).transpose()?;
    let mut normalized = BTreeMap::new();
    for (raw, record) in data.accounts {
        let account = account_id(&raw)?;
        ensure!(normalized.insert(account, record).is_none(), "备份中存在规范化后重复的账号");
    }
    if let Some(account) = &selected {
        ensure!(normalized.contains_key(account), "备份中没有账号 {account}");
    }
    let mut updated = auth.clone();
    let mut count = 0;
    for (account, record) in normalized {
        if selected.as_ref().is_some_and(|selected| selected != &account) { continue; }
        import_record(&mut updated, &account, record.seed, force)?;
        count += 1;
    }
    Ok((updated, count))
}

/// 完整校验备份和全部冲突后才保存, 失败不会导入前半部分记录.
pub fn import_file(file: &Path, account: Option<&str>, force: bool) -> Result<()> {
    let bytes = fs::read(file).with_context(|| format!("读取 seed 备份失败: {}", file.display()))?;
    // 不将 JSON 解析错误的内容带进日志, 避免损坏输入中的敏感值被打印.
    let data = serde_json::from_slice(&bytes).map_err(|_| anyhow::anyhow!("seed 备份 JSON 格式无效"))?;
    let (auth, count) = restore(&Auth::load()?, data, account, force)?;
    auth.save().context("保存恢复的 seed 记录失败")?;
    info!(accounts = count, "已恢复 seed 备份并重新派生密钥");
    println!("已导入 {count} 个账号, 密钥已派生但尚未验证微信密文");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn restore_conflicts_versions_and_aliases_never_mutate_original() {
        let mut auth = Auth::default();
        import_record(&mut auth, "wxid_z", 42, false).unwrap();
        let original = serde_json::to_vec(&auth).unwrap();
        let data = Backup { version: 1, accounts: BTreeMap::from([
            ("wxid_a".into(), SeedRecord { seed: 43 }), ("wxid_z".into(), SeedRecord { seed: 44 }),
        ]) };
        assert!(restore(&auth, data, None, false).is_err());
        assert_eq!(serde_json::to_vec(&auth).unwrap(), original);
        let mut data = backup(&auth, None).unwrap();
        data.version = 2;
        assert!(restore(&auth, data, None, true).is_err());
        let data = Backup { version: 1, accounts: BTreeMap::from([
            ("wxid_z".into(), SeedRecord { seed: 43 }), ("wxid_z_7e1e".into(), SeedRecord { seed: 44 }),
        ]) };
        assert!(restore(&auth, data, None, true).is_err());
        assert_eq!(serde_json::to_vec(&auth).unwrap(), original);
    }
}
