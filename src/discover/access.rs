//! 目录访问探测, 保留权限错误并让操作系统处理访问授权.
use std::fs::{self, ReadDir};
use std::io;
use std::path::Path;

use anyhow::{Context, Result};
#[cfg(not(target_os = "macos"))]
use tracing::debug;

/// 通用发现只调用平台接口, macOS 的访问请求与授权提示在 platform/macos 中.
#[cfg(target_os = "macos")]
pub(super) fn read_directory(path: &Path) -> Result<Option<ReadDir>> {
    crate::platform::macos::read_directory(path)
}

#[cfg(not(target_os = "macos"))]
pub(super) fn read_directory(path: &Path) -> Result<Option<ReadDir>> {
    debug!(path = %path.display(), "请求读取目录");
    match fs::read_dir(path) {
        Ok(entries) => Ok(Some(entries)),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(err).with_context(|| format!("读取目录失败: {}", path.display())),
    }
}

/// 只有路径不存在才返回 false, 权限和其他 IO 错误必须报告.
pub(super) fn is_directory(path: &Path) -> Result<bool> {
    match fs::metadata(path) {
        Ok(metadata) => Ok(metadata.is_dir()),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(err) => Err(err).with_context(|| format!("无法检查目录: {}", path.display())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[cfg(unix)]
    #[test]
    fn denied_directory_keeps_original_io_error() {
        use std::os::unix::fs::PermissionsExt;
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let parent = Path::new(env!("CARGO_MANIFEST_DIR")).join(".tmp");
        let root = parent.join(format!("access-denied-{}-{nonce}", std::process::id()));
        fs::create_dir_all(&root).unwrap();
        fs::set_permissions(&root, fs::Permissions::from_mode(0o000)).unwrap();
        let result = read_directory(&root);
        fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).unwrap();
        assert_eq!(root.parent(), Some(parent.as_path()));
        fs::remove_dir_all(&root).unwrap();
        // 特权测试环境能绕过 POSIX 位, 不能用它模拟 macOS TCC.
        if let Err(err) = result {
            let original = err.downcast_ref::<io::Error>().unwrap();
            assert_eq!(original.kind(), io::ErrorKind::PermissionDenied);
        }
    }

    #[test]
    fn missing_directory_is_optional_but_non_directory_is_an_error() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let parent = Path::new(env!("CARGO_MANIFEST_DIR")).join(".tmp");
        let root = parent.join(format!("access-test-{}-{nonce}", std::process::id()));
        assert!(read_directory(&root).unwrap().is_none());
        fs::create_dir_all(&root).unwrap();
        assert!(read_directory(&root).unwrap().is_some());
        let file = root.join("ordinary-file");
        fs::write(&file, b"fixture").unwrap();
        assert!(read_directory(&file).is_err());
        assert!(!is_directory(&file).unwrap());
        assert_eq!(root.parent(), Some(parent.as_path()));
        fs::remove_dir_all(root).unwrap();
    }
}
