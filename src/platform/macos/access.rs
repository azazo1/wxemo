//! macOS 容器访问请求. 系统决定是否弹出授权提示, 不使用私有 TCC API.
use std::collections::HashSet;
use std::fs::{self, ReadDir};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use anyhow::{Context, Result};
use tracing::{debug, info};

/// 已经提示过访问请求的目录.
///
/// 同一个目录在一次运行里只提示一次: 目录发现会对同一个容器路径反复调用这里,
/// 每次都打 INFO 会把终端刷满, 而用户其实只需要看到一次.
fn announced() -> &'static Mutex<HashSet<PathBuf>> {
    static ANNOUNCED: OnceLock<Mutex<HashSet<PathBuf>>> = OnceLock::new();
    ANNOUNCED.get_or_init(|| Mutex::new(HashSet::new()))
}

/// 主动请求读取目录. macOS 14+ 可能在访问其他应用的容器时请求授权.
/// 拒绝或已有的授权决定仍由系统处理, 不强制重置权限或自行授予完全磁盘访问.
pub(crate) fn read_directory(path: &Path) -> Result<Option<ReadDir>> {
    let first_time = announced()
        .lock()
        .map(|mut seen| seen.insert(path.to_path_buf()))
        .unwrap_or(false);
    if first_time {
        info!(path = %path.display(), "请求目录访问, 若 macOS 显示授权提示请决定是否允许");
    } else {
        debug!(path = %path.display(), "再次读取同一目录");
    }

    match fs::read_dir(path) {
        Ok(entries) => Ok(Some(entries)),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(err) => {
            let context = if permission_denied(&err) {
                format!(
                    "macOS 拒绝访问目录: {}. 如果系统显示容器访问请求, 请决定是否允许. \
                     若此前已拒绝, 请检查运行 wxemo 的终端宿主的隐私权限后重试; \
                     本程序不会重置系统授权或自动获取完全磁盘访问权限",
                    path.display()
                )
            } else {
                format!("读取目录失败: {}", path.display())
            };
            Err(err).context(context)
        }
    }
}

fn permission_denied(err: &io::Error) -> bool {
    // Darwin 的 EPERM=1, EACCES=13; 保留底层错误作为错误链原因.
    err.kind() == io::ErrorKind::PermissionDenied || matches!(err.raw_os_error(), Some(1 | 13))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognizes_darwin_access_denials_but_not_missing_paths() {
        assert!(permission_denied(&io::Error::from_raw_os_error(1)));
        assert!(permission_denied(&io::Error::from_raw_os_error(13)));
        assert!(!permission_denied(&io::Error::from_raw_os_error(2)));
    }
}
