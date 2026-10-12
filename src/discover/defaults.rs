//! 各平台微信 4.x 默认数据目录的有限范围探测.
use std::path::{Path, PathBuf};
use anyhow::Result;
use tracing::info;
#[cfg(any(target_os = "windows", target_os = "linux"))]
use tracing::warn;

#[cfg(any(target_os = "macos", test))]
fn macos_candidates(home: &Path) -> Vec<PathBuf> {
    vec![home.join("Library/Containers/com.tencent.xinWeChat/Data/Documents/xwechat_files")]
}

#[cfg(any(target_os = "windows", target_os = "linux", test))]
fn document_candidates(documents: &Path) -> Vec<PathBuf> {
    ["xwechat_files", "Tencent/Weixin/xwechat_files", "Tencent/WeChat/xwechat_files", "WeChat Files/xwechat_files"]
        .iter().map(|relative| documents.join(relative)).collect()
}

#[cfg(any(target_os = "windows", test))]
fn windows_candidates(home: &Path, documents: &Path, onedrive: &[PathBuf]) -> Vec<PathBuf> {
    let mut candidates = document_candidates(documents);
    candidates.extend(document_candidates(&home.join("Documents")));
    for root in onedrive { candidates.extend(document_candidates(&root.join("Documents"))); }
    candidates.push(home.join("xwechat_files"));
    candidates
}

#[cfg(any(target_os = "linux", test))]
fn linux_candidates(home: &Path, documents: &Path, config: &Path, data: &Path) -> Vec<PathBuf> {
    let mut candidates = document_candidates(documents);
    candidates.extend(document_candidates(&home.join("Documents")));
    candidates.extend([
        home.join("xwechat_files"),
        config.join("wechat/xwechat_files"),
        data.join("wechat/home/xwechat_files"),
    ]);
    candidates
}

#[cfg(any(target_os = "linux", test))]
fn parse_xdg_documents(text: &str, home: &Path) -> Option<PathBuf> {
    for line in text.lines() {
        let Some(value) = line.trim().strip_prefix("XDG_DOCUMENTS_DIR=") else { continue };
        let value = value.trim().strip_prefix('"')?.strip_suffix('"')?;
        let path = if let Some(tail) = value.strip_prefix("$HOME/").or_else(|| value.strip_prefix("${HOME}/")) {
            home.join(tail)
        } else if value == "$HOME" || value == "${HOME}" {
            home.to_path_buf()
        } else if !value.contains('$') && Path::new(value).is_absolute() {
            PathBuf::from(value)
        } else { return None };
        return Some(path);
    }
    None
}

#[cfg(target_os = "linux")]
fn linux_documents(home: &Path, config: &Path) -> PathBuf {
    if let Some(path) = std::env::var_os("XDG_DOCUMENTS_DIR").map(PathBuf::from).filter(|path| path.is_absolute()) {
        return path;
    }
    let file = config.join("user-dirs.dirs");
    match std::fs::read_to_string(&file) {
        Ok(text) => if let Some(path) = parse_xdg_documents(&text, home) { return path },
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {},
        Err(err) => warn!(path = %file.display(), error = %err, "无法读取 XDG 文档目录, 使用默认候选"),
    }
    home.join("Documents")
}

/// 使用系统 Known Folder API, 支持 Documents 被重定向到 OneDrive 或其他位置.
#[cfg(target_os = "windows")]
fn windows_documents(home: &Path) -> PathBuf {
    use std::os::windows::ffi::OsStringExt;
    use windows_sys::Win32::System::Com::CoTaskMemFree;
    use windows_sys::Win32::UI::Shell::{FOLDERID_Documents, SHGetKnownFolderPath};
    let mut output = std::ptr::null_mut();
    // SAFETY: 系统在成功时返回以 NUL 结尾的 UTF-16 缓冲区, 由 CoTaskMemFree 释放.
    unsafe {
        let result = SHGetKnownFolderPath(&FOLDERID_Documents, 0, std::ptr::null_mut(), &mut output);
        if result >= 0 && !output.is_null() {
            let mut length = 0;
            while *output.add(length) != 0 { length += 1; }
            let path = PathBuf::from(std::ffi::OsString::from_wide(std::slice::from_raw_parts(output, length)));
            CoTaskMemFree(output.cast());
            return path;
        }
        if !output.is_null() { CoTaskMemFree(output.cast()); }
        warn!(result, "无法读取 Windows Documents 位置, 使用默认候选");
    }
    home.join("Documents")
}

fn platform_candidates() -> Vec<PathBuf> {
    let Some(home) = std::env::var_os("USERPROFILE").filter(|_| cfg!(windows)).or_else(|| std::env::var_os("HOME")).map(PathBuf::from) else { return Vec::new() };
    #[cfg(target_os = "macos")]
    let candidates = macos_candidates(&home);
    #[cfg(target_os = "windows")]
    let candidates = {
        let onedrive: Vec<PathBuf> = ["OneDrive", "OneDriveConsumer", "OneDriveCommercial"].iter()
            .filter_map(std::env::var_os).map(PathBuf::from).collect();
        windows_candidates(&home, &windows_documents(&home), &onedrive)
    };
    #[cfg(target_os = "linux")]
    let candidates = {
        let config = std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from).filter(|path| path.is_absolute()).unwrap_or_else(|| home.join(".config"));
        let data = std::env::var_os("XDG_DATA_HOME").map(PathBuf::from).filter(|path| path.is_absolute()).unwrap_or_else(|| home.join(".local/share"));
        linux_candidates(&home, &linux_documents(&home, &config), &config, &data)
    };
    #[cfg(not(any(target_os = "macos", target_os = "windows", target_os = "linux")))]
    let candidates = { let _ = home; Vec::new() };
    candidates
}

/// 仅采用包含支持的表情目录布局的候选, 并按真实路径去重.
fn detect(candidates: &[PathBuf]) -> Result<Vec<PathBuf>> {
    let mut found = Vec::new();
    let mut seen = Vec::new();
    for candidate in candidates {
        if !candidate.is_dir() { continue; }
        let canonical = candidate.canonicalize()?;
        if seen.contains(&canonical) { continue; }
        seen.push(canonical);
        let accounts = super::find_accounts(std::slice::from_ref(candidate))?;
        if !accounts.is_empty() {
            info!(root = %candidate.display(), accounts = accounts.len(), "自动发现微信数据目录");
            found.push(candidate.clone());
        }
    }
    Ok(found)
}

/// 显式参数优先于配置, 只有两者都为空时探测默认位置.
pub fn resolve(explicit: &[String], configured: &[PathBuf]) -> Result<Vec<PathBuf>> {
    if !explicit.is_empty() { return Ok(explicit.iter().map(|raw| super::normalize_root(raw)).collect()); }
    if !configured.is_empty() { return Ok(configured.to_vec()); }
    detect(&platform_candidates())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn detects_all_platform_layouts_and_respects_explicit_roots() {
        let nonce = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
        let parent = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(".tmp");
        let home = parent.join(format!("default-discovery-{}-{nonce}", std::process::id()));
        let mut candidates = macos_candidates(&home);
        candidates.extend(windows_candidates(&home, &home.join("RedirectedDocs"), &[home.join("OneDrive")]));
        candidates.extend(linux_candidates(&home, &home.join("XdgDocs"), &home.join("Config"), &home.join("Data")));
        assert!(detect(&candidates).unwrap().is_empty());
        for (index, root) in candidates.iter().enumerate() {
            fs::create_dir_all(root.join(format!("wxid_test{index}_7e1e/business/emoticon"))).unwrap();
        }
        let detected = detect(&candidates).unwrap();
        let mut distinct = candidates.clone();
        distinct.sort(); distinct.dedup();
        assert_eq!(detected.len(), distinct.len());
        assert_eq!(super::super::find_accounts(&detected).unwrap().len(), candidates.len());
        let configured = vec![home.join("custom")];
        assert_eq!(resolve(&[], &configured).unwrap(), configured);
        let explicit = vec![home.join("explicit").display().to_string()];
        assert_eq!(resolve(&explicit, &configured).unwrap(), vec![PathBuf::from(&explicit[0])]);
        assert_eq!(home.parent(), Some(parent.as_path()));
        fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn xdg_documents_expands_home_without_evaluating_shell() {
        let home = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        assert_eq!(parse_xdg_documents("XDG_DOCUMENTS_DIR=\"$HOME/文档\"", &home), Some(home.join("文档")));
        assert_eq!(parse_xdg_documents("XDG_DOCUMENTS_DIR=\"${HOME}/Documents\"", &home), Some(home.join("Documents")));
        assert!(parse_xdg_documents("XDG_DOCUMENTS_DIR=\"$(touch bad)\"", &home).is_none());
        assert!(parse_xdg_documents("XDG_DOCUMENTS_DIR=\"relative\"", &home).is_none());
    }
}
