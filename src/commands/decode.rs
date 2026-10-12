//! 已解密 WXGF 文件的单文件与批量转换.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{bail, ensure, Context, Result};
use tracing::{debug, info, warn};

use crate::emoticon::wxgf::{self, native, Header, ImageInfo, MAX_INPUT_BYTES, MAX_OUTPUT_BYTES};

pub struct DecodeOptions {
    pub input: PathBuf,
    pub out: PathBuf,
    pub decoder_lib: Option<PathBuf>,
    pub timeout_secs: u64,
}

pub fn run(options: DecodeOptions) -> Result<()> {
    ensure!(options.timeout_secs > 0, "单文件超时必须大于零");
    let input = options
        .input
        .canonicalize()
        .context("输入文件或目录不存在")?;
    let directory = input.is_dir();
    ensure!(directory || input.is_file(), "输入不是普通文件或目录");
    let output = resolve_output(&options.out)?;
    if directory {
        ensure!(
            !output.starts_with(&input),
            "输出目录不能等于输入目录或位于其内部"
        );
    }
    let mut files = Vec::new();
    if directory {
        collect_files(&input, &mut files)?;
    } else {
        files.push(input.clone());
    }
    files.sort();
    ensure!(!files.is_empty(), "输入目录中没有 .wxgf 文件");
    let library = native::find_library(options.decoder_lib.as_deref())?;
    info!(count = files.len(), library = %library.display(), "开始完整 WXGF 解码, 动画保留为 APNG");
    fs::create_dir_all(&output).context("创建输出目录失败")?;
    let output = output.canonicalize().context("解析输出目录失败")?;
    let temp_root = output.join(".tmp");
    ensure!(
        resolve_output(&temp_root)?.starts_with(&output),
        "工作目录符号链接指向输出区域之外"
    );
    fs::create_dir_all(&temp_root).context("创建工作目录失败")?;
    ensure!(
        temp_root.canonicalize()?.starts_with(&output),
        "工作目录符号链接指向输出区域之外"
    );
    let executable = std::env::current_exe().context("无法定位工作进程程序")?;
    let mut succeeded = 0;
    let mut failed = 0;
    let mut animated = 0;
    let started = Instant::now();
    for (index, path) in files.iter().enumerate() {
        let relative = if directory {
            path.strip_prefix(&input).context("输入路径超出目录")?
        } else {
            Path::new(path.file_name().context("输入文件名无效")?)
        };
        let target = output.join(relative).with_extension("png");
        let result = convert_file(
            path,
            &target,
            &output,
            &temp_root,
            &executable,
            &library,
            options.timeout_secs,
        );
        match result {
            Ok(details) => {
                succeeded += 1;
                animated += usize::from(details.frames > 1);
                debug!(path = %path.display(), out = %target.display(), width = details.width,
                    height = details.height, frames = details.frames, plays = ?details.plays,
                    duration_ms = details.duration_ms, "PNG/APNG 已写出");
            }
            Err(error) => {
                failed += 1;
                warn!(path = %path.display(), error = %format!("{error:#}"), "转换失败, 原文件保持不变");
            }
        }
        if (index + 1) % 10 == 0 || index + 1 == files.len() {
            info!(
                processed = index + 1,
                total = files.len(),
                succeeded,
                failed,
                "WXGF 解码进度"
            );
        }
    }
    info!(
        succeeded,
        failed,
        animated,
        elapsed_secs = started.elapsed().as_secs_f64(),
        "WXGF 解码结束"
    );
    println!("转换成功 {succeeded} 个, 其中动画 {animated} 个, 失败 {failed} 个");
    println!("输出目录: {}", output.display());
    ensure!(
        failed == 0,
        "有 {failed} 个 WXGF 转换失败, 已成功的结果保留, 详情见警告"
    );
    Ok(())
}

fn read_bounded(path: &Path, limit: usize) -> Result<Vec<u8>> {
    let file = File::open(path).with_context(|| format!("读取失败: {}", path.display()))?;
    ensure!(
        file.metadata()?.len() <= limit as u64,
        "文件大小超过限制: {}",
        path.display()
    );
    let mut data = Vec::new();
    file.take(limit as u64 + 1).read_to_end(&mut data)?;
    ensure!(data.len() <= limit, "读取过程中文件大小超过限制");
    Ok(data)
}

fn resolve_output(path: &Path) -> Result<PathBuf> {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()?.join(path)
    };
    let mut ancestor = absolute.as_path();
    let mut missing = Vec::new();
    while !ancestor.exists() {
        missing.push(ancestor.file_name().context("输出路径无效")?.to_os_string());
        ancestor = ancestor.parent().context("输出路径缺少有效父目录")?;
    }
    let mut resolved = ancestor.canonicalize().context("无法解析输出路径")?;
    for name in missing.into_iter().rev() {
        resolved.push(name);
    }
    Ok(resolved)
}

fn collect_files(root: &Path, files: &mut Vec<PathBuf>) -> Result<()> {
    for entry in fs::read_dir(root).with_context(|| format!("读取目录失败: {}", root.display()))?
    {
        let entry = entry?;
        let kind = entry.file_type()?;
        let path = entry.path();
        if kind.is_dir() {
            collect_files(&path, files)?;
        } else if kind.is_file()
            && path
                .extension()
                .is_some_and(|ext| ext.eq_ignore_ascii_case("wxgf"))
        {
            files.push(path);
        }
        // 不跟随目录内的符号链接, 防止递归循环和越界读取.
    }
    Ok(())
}

fn convert_file(
    input: &Path,
    target: &Path,
    output: &Path,
    temp_root: &Path,
    executable: &Path,
    library: &Path,
    timeout_secs: u64,
) -> Result<ImageInfo> {
    ensure!(
        !target.try_exists()?,
        "输出文件已存在, 拒绝覆盖: {}",
        target.display()
    );
    let source = Header::parse(&read_bounded(input, MAX_INPUT_BYTES)?)?;
    let work = tempfile::Builder::new()
        .prefix("wxgf-")
        .tempdir_in(temp_root)?;
    let intermediate = work.path().join("decoded.png");
    let child = Command::new(executable)
        .arg("--no-interactive")
        .arg("__decode-wxgf")
        .arg("--input")
        .arg(input)
        .arg("--out")
        .arg(&intermediate)
        .arg("--decoder-lib")
        .arg(library)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .context("启动原生解码工作进程失败")?;
    wait_worker(child, Duration::from_secs(timeout_secs))?;
    let mut png = read_bounded(&intermediate, MAX_OUTPUT_BYTES)?;
    let details = wxgf::validate_png(&mut png, source)?;
    publish(&png, target, output)?;
    Ok(details)
}

struct ChildGuard(Child);

impl Drop for ChildGuard {
    fn drop(&mut self) {
        // 等待状态已经缓存时不再发送终止信号.
        if self.0.try_wait().ok().flatten().is_none() {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
}

fn wait_worker(child: Child, timeout: Duration) -> Result<()> {
    let mut child = ChildGuard(child);
    let mut reader = child.0.stderr.take().context("工作进程缺少错误输出管道")?;
    let capture = thread::spawn(move || {
        let mut captured = Vec::new();
        // 超过 64 KiB 的日志仍持续排空, 避免管道阻塞, 但不无限增长内存.
        let _ = reader.by_ref().take(64 * 1024).read_to_end(&mut captured);
        let _ = io::copy(&mut reader, &mut io::sink());
        captured
    });
    let started = Instant::now();
    let status = loop {
        match child.0.try_wait() {
            Ok(Some(status)) => break Ok(status),
            Ok(None) if started.elapsed() < timeout => thread::sleep(Duration::from_millis(25)),
            Ok(None) => {
                break Err(anyhow::anyhow!(
                    "原生解码超过 {} 秒, 已终止工作进程",
                    timeout.as_secs()
                ))
            }
            Err(error) => break Err(error.into()),
        }
    };
    if status.is_err() {
        let _ = child.0.kill();
        let _ = child.0.wait();
    }
    let captured = capture.join().unwrap_or_default();
    let status = status?;
    if !status.success() {
        bail!(
            "原生解码工作进程失败 ({status}): {}",
            String::from_utf8_lossy(&captured).trim()
        );
    }
    Ok(())
}

fn publish(data: &[u8], target: &Path, output: &Path) -> Result<()> {
    let parent = target.parent().context("输出文件缺少父目录")?;
    ensure!(
        resolve_output(parent)?.starts_with(output),
        "输出子目录符号链接指向输出区域之外"
    );
    fs::create_dir_all(parent)?;
    ensure!(
        parent.canonicalize()?.starts_with(output),
        "输出子目录符号链接指向输出区域之外"
    );
    let temporary = parent.join(".tmp");
    fs::create_dir_all(&temporary)?;
    ensure!(
        temporary.canonicalize()?.starts_with(output),
        "临时目录符号链接指向输出区域之外"
    );
    let mut file = tempfile::NamedTempFile::new_in(temporary)?;
    file.write_all(data)?;
    file.as_file().sync_all()?;
    file.persist_noclobber(target)
        .map_err(|error| error.error)
        .with_context(|| format!("发布图片失败, 不覆盖已有文件: {}", target.display()))?;
    Ok(())
}

/// 内部工作进程入口, 不加载账号配置或密钥.
pub fn run_worker(input: &Path, output: &Path, library: &Path) -> Result<()> {
    let data = read_bounded(input, MAX_INPUT_BYTES)?;
    let (png, _) = native::decode(&data, library)?;
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(output)
        .context("创建隔离工作进程输出失败")?;
    file.write_all(&png)?;
    file.sync_all()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn publish_does_not_overwrite_and_rejects_escape() {
        let project = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(".tmp");
        fs::create_dir_all(&project).unwrap();
        let root = tempfile::tempdir_in(project).unwrap();
        let output = root.path().join("out");
        fs::create_dir_all(&output).unwrap();
        let output = output.canonicalize().unwrap();
        let target = output.join("image.png");
        publish(b"first", &target, &output).unwrap();
        assert!(publish(b"second", &target, &output).is_err());
        assert_eq!(fs::read(target).unwrap(), b"first");
        #[cfg(unix)]
        {
            let outside = root.path().join("outside");
            fs::create_dir_all(&outside).unwrap();
            std::os::unix::fs::symlink(&outside, output.join("link")).unwrap();
            assert!(publish(b"escape", &output.join("link/image.png"), &output).is_err());
            assert!(!outside.join("image.png").exists());
        }
    }
}
