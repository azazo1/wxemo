//! 在独立工作进程中加载用户已有的 WXAM 媒体库.

use std::path::{Path, PathBuf};

use anyhow::{bail, ensure, Context, Result};
use libloading::Library;
use tracing::debug;

use super::{validate_png, Header, ImageInfo, MAX_OUTPUT_BYTES};

// _5 是版本化 C ABI. 输入长度和输出容量均为 32 位整数.
type DecodeFn = unsafe extern "C" fn(*const u8, i32, *mut u8, *mut i32, *const i32) -> i32;

pub fn find_library(explicit: Option<&Path>) -> Result<PathBuf> {
    if let Some(path) = explicit {
        return checked_library_path(path);
    }
    #[cfg(target_os = "macos")]
    {
        let suffix =
            Path::new("WeChat.app/Contents/Frameworks/MultiMediaDyn.framework/MultiMediaDyn");
        let mut roots = vec![PathBuf::from("/Applications")];
        if let Some(home) = std::env::var_os("HOME") {
            roots.push(PathBuf::from(home).join("Applications"));
        }
        for root in roots {
            let path = root.join(suffix);
            if path.is_file() {
                return checked_library_path(&path);
            }
        }
    }
    #[cfg(windows)]
    {
        let mut candidates = Vec::new();
        for name in [
            "ProgramFiles",
            "ProgramFiles(x86)",
            "ProgramW6432",
            "LOCALAPPDATA",
        ] {
            if let Some(root) = std::env::var_os(name) {
                for app in ["Weixin", "WeChat"] {
                    let root = PathBuf::from(&root).join("Tencent").join(app);
                    candidates.push(root.join("VoipEngine.dll"));
                    if let Ok(entries) = std::fs::read_dir(root) {
                        for entry in entries.flatten() {
                            if entry.file_type().is_ok_and(|kind| kind.is_dir()) {
                                candidates.push(entry.path().join("VoipEngine.dll"));
                            }
                        }
                    }
                }
            }
        }
        candidates.retain(|path| path.is_file());
        candidates.sort_by_key(|path| std::fs::metadata(path).and_then(|m| m.modified()).ok());
        if let Some(path) = candidates.last() {
            return checked_library_path(path);
        }
    }
    bail!("找不到微信原生 WXAM 解码库. macOS 需要已有的 WeChat.app, Windows 需要已有的 VoipEngine.dll; 自定义位置或 Linux 使用 --decoder-lib 指定兼容库. 不会退化成仅导出首帧")
}

fn checked_library_path(path: &Path) -> Result<PathBuf> {
    ensure!(
        path.is_file(),
        "解码库不存在或不是普通文件: {}",
        path.display()
    );
    path.canonicalize()
        .with_context(|| format!("无法解析解码库路径: {}", path.display()))
}

unsafe fn load_library(path: &Path) -> Result<Library> {
    #[cfg(windows)]
    {
        // 只从库所在目录和系统默认安全目录解析 DLL 依赖, 不修改全局 PATH.
        let flags = 0x0000_0100 | 0x0000_1000;
        Ok(unsafe { libloading::os::windows::Library::load_with_flags(path, flags)? }.into())
    }
    #[cfg(not(windows))]
    {
        Ok(unsafe { Library::new(path)? })
    }
}

/// 只能从隔离的工作进程调用, 避免私有库的崩溃影响整个批量任务.
pub fn decode(data: &[u8], path: &Path) -> Result<(Vec<u8>, ImageInfo)> {
    let header = Header::parse(data)?;
    // 安全前提: 用户指定或本机微信提供的库导出版本化 _5 接口.
    // Library 的生命周期覆盖函数调用, 所有缓冲区在调用期间保持有效.
    let library = unsafe { load_library(path) }
        .with_context(|| format!("加载解码库失败, 请检查库的架构和依赖: {}", path.display()))?;
    let function = unsafe { library.get::<DecodeFn>(b"wxam_dec_wxam2pic_5\0") }
        .context("解码库没有兼容的 wxam_dec_wxam2pic_5 接口")?;
    let mut output = vec![0u8; MAX_OUTPUT_BYTES];
    let mut length = MAX_OUTPUT_BYTES as i32;
    // mode=2 输出 PNG/APNG, 保留多帧, 逐帧时序和完整 alpha.
    // 其余选项保持零, 预留空间覆盖已有 _5 实现的扩展字段.
    let mut options = [0i32; 32];
    options[0] = 2;
    let input_length = i32::try_from(data.len()).context("输入长度溢出")?;
    let status = unsafe {
        function(
            data.as_ptr(),
            input_length,
            output.as_mut_ptr(),
            &mut length,
            options.as_ptr(),
        )
    };
    ensure!(
        status == 0,
        "WXAM 原生解码失败: 状态 {status}, 输出容量 {} MiB",
        MAX_OUTPUT_BYTES / (1024 * 1024)
    );
    ensure!(
        length > 0 && (length as usize) <= output.len(),
        "解码器返回了无效的输出长度"
    );
    output.truncate(length as usize);
    let info = validate_png(&mut output, header)?;
    debug!(
        width = info.width,
        height = info.height,
        frames = info.frames,
        "原生解码结果已通过容器校验"
    );
    Ok((output, info))
}
