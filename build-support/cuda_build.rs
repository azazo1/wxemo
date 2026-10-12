//! 构建期把 `kernel.cu` 编译成 cubin, 供 host 侧用驱动 API 加载.
//!
//! 为什么是 cubin 而不是 PTX: CUDA 13.4 产出的 PTX 是 9.4 版本, 高于当前驱动
//! (CUDA 13.0) 支持的版本, `cuModuleLoadData` 会返回 CUDA_ERROR_UNSUPPORTED_PTX_VERSION.
//! cubin 已经是落到 SASS 的镜像, 由驱动直接加载, 不受 PTX 版本限制.
//!
//! 三种构建环境:
//!
//! - Windows 原生: 生成 .bat 加载 vcvars64.bat 后调用 nvcc.exe.
//! - WSL: 本机没有 nvcc, 通过 interop 调用 Windows 的 cmd.exe, 在 .bat 里加载
//!   vcvars64.bat 并调用 nvcc.exe. 源码先复制到 Windows 可见的临时目录,
//!   产物再复制回 OUT_DIR.
//! - 其他平台: 明确报错, 不做静默跳过, 否则用户会以为 CUDA 后端编进去了.
//!
//! WSL 这条路上有两个很容易踩的坑, 代码里都绕开了:
//!
//! 1. interop 只转换**可执行文件**的路径, 不转换普通参数. 把 `/mnt/c/.../x.bat`
//!    当参数传给 cmd.exe 会得到 "系统找不到指定的路径", 必须自己转成 `C:\...`.
//!    同理, .bat 内容里的路径是给 Windows 看的, 也得是 `C:\...`.
//! 2. cmd.exe 不接受 UNC 路径作为当前目录. 构建脚本的工作目录必须是
//!    `/mnt/<盘>/...`, 否则 WSL 会把 Linux 侧工作目录以 `\\wsl.localhost\...`
//!    的形式交给 Windows, cmd.exe 直接拒绝执行.
//!
//! 可用环境变量覆盖探测结果:
//!
//! - `WXEMO_CUDA_NVCC`: nvcc 可执行文件路径.
//! - `WXEMO_CUDA_VCVARS`: vcvars64.bat 路径.
//! - `WXEMO_CUDA_ARCH`: 目标架构, 默认 `sm_89`.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

/// 源码位置, 相对 Cargo 包根目录.
const KERNEL_SOURCE: &str = "src/emoticon/search/cuda/kernel.cu";

/// 产物名字, host.rs 用 `include_bytes!` 引用同一个名字.
const KERNEL_OUTPUT: &str = "wxemo_cuda_kernel.cubin";

/// 默认目标架构. 默认值对应 Ada (RTX 40 系), 可用 `WXEMO_CUDA_ARCH` 覆盖.
const DEFAULT_ARCH: &str = "sm_89";

/// 编译内核; 失败时以构建错误的形式抛出, 不做降级.
pub fn compile() {
    let manifest = PathBuf::from(env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"));
    let source = manifest.join(KERNEL_SOURCE);
    let out_dir = PathBuf::from(env::var("OUT_DIR").expect("OUT_DIR"));
    let output = out_dir.join(KERNEL_OUTPUT);

    println!("cargo:rerun-if-changed={}", source.display());
    println!("cargo:rerun-if-env-changed=WXEMO_CUDA_NVCC");
    println!("cargo:rerun-if-env-changed=WXEMO_CUDA_VCVARS");
    println!("cargo:rerun-if-env-changed=WXEMO_CUDA_ARCH");

    let arch = env::var("WXEMO_CUDA_ARCH").unwrap_or_else(|_| DEFAULT_ARCH.to_string());
    let nvcc = find_nvcc().unwrap_or_else(|| {
        panic!("找不到 nvcc, 请安装 CUDA Toolkit, 或用 WXEMO_CUDA_NVCC 指定 nvcc 的完整路径")
    });
    let vcvars = find_vcvars();

    if cfg!(windows) {
        compile_native(&source, &output, &nvcc, vcvars.as_deref(), &arch);
    } else if cfg!(target_os = "linux") {
        compile_through_windows(&source, &output, &nvcc, vcvars.as_deref(), &arch);
    } else {
        panic!(
            "cuda feature 目前只支持 Windows 与 WSL (通过 interop 调 Windows nvcc), \
             当前目标平台 {}. 请去掉 --features cuda",
            env::var("TARGET").unwrap_or_else(|_| "unknown".into())
        );
    }

    if !output.is_file() {
        panic!("nvcc 报告成功但 {} 不存在", output.display());
    }
}

/// 找 nvcc: 环境变量 > `CUDA_PATH` > 常见安装目录下版本号最大的一个.
fn find_nvcc() -> Option<PathBuf> {
    if let Some(path) = env::var_os("WXEMO_CUDA_NVCC") {
        let path = PathBuf::from(path);
        if path.is_file() {
            return Some(path);
        }
    }

    let mut roots: Vec<PathBuf> = Vec::new();
    if let Some(base) = env::var_os("CUDA_PATH") {
        roots.push(PathBuf::from(base));
    }
    for base in toolkit_roots() {
        if !base.is_dir() {
            continue;
        }
        let mut versions: Vec<PathBuf> = fs::read_dir(&base)
            .into_iter()
            .flatten()
            .flatten()
            .map(|entry| entry.path())
            .filter(|path| path.is_dir())
            .collect();
        // 目录名形如 v13.4, 倒序让版本号大的排前面.
        versions.sort();
        versions.reverse();
        roots.append(&mut versions);
    }

    for root in roots {
        for name in ["nvcc.exe", "nvcc"] {
            let candidate = root.join("bin").join(name);
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    None
}

/// CUDA Toolkit 的候选安装根目录.
fn toolkit_roots() -> Vec<PathBuf> {
    if cfg!(windows) {
        vec![
            PathBuf::from("C:/Program Files/NVIDIA GPU Computing Toolkit/CUDA"),
            PathBuf::from("C:/Program Files (x86)/NVIDIA GPU Computing Toolkit/CUDA"),
        ]
    } else {
        // WSL 通过 /mnt/c 访问 Windows 侧的安装.
        vec![
            PathBuf::from("/mnt/c/Program Files/NVIDIA GPU Computing Toolkit/CUDA"),
            PathBuf::from("/mnt/c/Program Files (x86)/NVIDIA GPU Computing Toolkit/CUDA"),
        ]
    }
}

/// 找 vcvars64.bat, 用于把 MSVC 的 cl.exe 与链接器放进 PATH.
///
/// 目录结构是 `<VS 根>/<年份>/<版本>/VC/Auxiliary/Build/vcvars64.bat`, 按这个
/// 形状直接枚举, 不做全树递归. 找不到时返回 None, 由调用方决定是否还能直接调 nvcc.
fn find_vcvars() -> Option<PathBuf> {
    if let Some(path) = env::var_os("WXEMO_CUDA_VCVARS") {
        let path = PathBuf::from(path);
        if path.is_file() {
            return Some(path);
        }
    }

    let roots: Vec<PathBuf> = if cfg!(windows) {
        vec![
            PathBuf::from("C:/Program Files/Microsoft Visual Studio"),
            PathBuf::from("C:/Program Files (x86)/Microsoft Visual Studio"),
        ]
    } else {
        vec![
            PathBuf::from("/mnt/c/Program Files/Microsoft Visual Studio"),
            PathBuf::from("/mnt/c/Program Files (x86)/Microsoft Visual Studio"),
        ]
    };

    let mut found: Vec<PathBuf> = Vec::new();
    for root in roots {
        for year in read_dirs(&root) {
            for edition in read_dirs(&year) {
                let candidate = edition.join("VC/Auxiliary/Build/vcvars64.bat");
                if candidate.is_file() {
                    found.push(candidate);
                }
            }
        }
    }
    // 路径里含版本号, 字典序最大的一般是最新的那套.
    found.sort();
    found.pop()
}

/// 列出一个目录下的子目录, 读不了就当空.
fn read_dirs(dir: &Path) -> Vec<PathBuf> {
    fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.is_dir())
        .collect()
}

/// Windows 原生: 生成 .bat 后交给 cmd.exe 执行.
fn compile_native(source: &Path, output: &Path, nvcc: &Path, vcvars: Option<&Path>, arch: &str) {
    let script_dir = env::temp_dir().join(format!("wxemo-cuda-{}", std::process::id()));
    fs::create_dir_all(&script_dir).expect("创建构建脚本目录失败");

    let script = script_dir.join("build-cuda.bat");
    let text = batch_script(
        &nvcc.display().to_string(),
        &source.display().to_string(),
        &output.display().to_string(),
        arch,
        vcvars.map(|path| path.display().to_string()).as_deref(),
    );
    fs::write(&script, text).expect("写入构建脚本失败");

    run_batch(Path::new("cmd.exe"), &script, &script_dir);
    let _ = fs::remove_dir_all(&script_dir);
}

/// WSL: 源码与产物先在 Windows 可见的目录里过一遍, 再复制回 OUT_DIR.
fn compile_through_windows(
    source: &Path,
    output: &Path,
    nvcc: &Path,
    vcvars: Option<&Path>,
    arch: &str,
) {
    // WSL 的 interop 只在绝对路径下可靠, cmd.exe 在 PATH 里通常找不到.
    let cmd = Path::new("/mnt/c/Windows/System32/cmd.exe");
    assert!(
        cmd.is_file(),
        "找不到 {}; cuda feature 在 Linux 上需要 WSL interop 才能调用 Windows 的 nvcc",
        cmd.display()
    );

    let nvcc_windows = to_windows_path(nvcc).unwrap_or_else(|| {
        panic!(
            "WSL 下 nvcc 必须位于 /mnt/<盘>/ 之下才能交给 Windows 执行, 实际 {}",
            nvcc.display()
        )
    });
    let vcvars_windows = vcvars.and_then(to_windows_path);

    let windows_temp = windows_temp_dir().unwrap_or_else(|| {
        panic!("无法从 cmd.exe 问到 %TEMP%, 可用 WXEMO_CUDA_VCVARS / WXEMO_CUDA_NVCC 绕开探测")
    });
    let staging = windows_temp.join(format!("wxemo-cuda-{}", std::process::id()));
    let staging_windows = to_windows_path(&staging).expect("暂存目录应当位于 /mnt/<盘>/ 之下");
    fs::create_dir_all(&staging).expect("创建 Windows 侧暂存目录失败");

    // nvcc.exe 是 Windows 程序, 它读写的路径必须是 Windows 形式.
    let source_windows = format!("{staging_windows}\\kernel.cu");
    let output_windows = format!("{staging_windows}\\{KERNEL_OUTPUT}");

    fs::copy(source, staging.join("kernel.cu")).expect("复制 kernel.cu 到 Windows 侧失败");

    let script = staging.join("build-cuda.bat");
    // .bat 的内容由 Windows 侧解释, 路径全部走 Windows 形式.
    fs::write(
        &script,
        batch_script(
            &nvcc_windows,
            &source_windows,
            &output_windows,
            arch,
            vcvars_windows.as_deref(),
        ),
    )
    .expect("写入构建脚本失败");

    // cmd.exe 的参数同样不做路径转换, 传 Windows 形式.
    run_batch(
        cmd,
        Path::new(&format!("{staging_windows}\\build-cuda.bat")),
        &staging,
    );

    let staged_output = staging.join(KERNEL_OUTPUT);
    assert!(
        staged_output.is_file(),
        "nvcc 没有产出 {}",
        staged_output.display()
    );
    fs::copy(&staged_output, output).expect("把 cubin 复制回 OUT_DIR 失败");
    let _ = fs::remove_dir_all(&staging);
}

/// 生成构建脚本内容.
///
/// 参数已经是 Windows 形式的路径, 直接嵌进脚本.
/// `chcp 65001` 与 `-Xcompiler /utf-8` 一起把源文件按 UTF-8 读, 否则
/// 中文注释会被按 GBK 解释并刷一屏 C4819.
fn batch_script(
    nvcc: &str,
    source: &str,
    output: &str,
    arch: &str,
    vcvars: Option<&str>,
) -> String {
    let mut script = String::new();
    script.push_str("@echo off\r\n");
    script.push_str("chcp 65001 >nul\r\n");
    if let Some(vcvars) = vcvars {
        script.push_str(&format!("call \"{vcvars}\" >nul 2>&1\r\n"));
        script.push_str("if errorlevel 1 exit /b 1\r\n");
    }
    script.push_str(&format!(
        "\"{nvcc}\" -cubin -arch={arch} -O3 -Xcompiler \"/utf-8\" -o \"{output}\" \"{source}\"\r\n"
    ));
    script.push_str("if errorlevel 1 exit /b 1\r\n");
    script.push_str("exit /b 0\r\n");
    script
}

/// 跑 .bat, 成功时保持安静, 失败时把 nvcc 的完整诊断带进构建错误.
///
/// `work_dir` 必须是 Windows 盘上的 Linux 路径: WSL interop 会把 Linux 侧的工作目录
/// 作为 `\\wsl.localhost\...` 这样的 UNC 路径交给 Windows, 而 cmd.exe 拒绝以 UNC
/// 路径作为当前目录, 于是脚本根本跑不起来.
fn run_batch(cmd: &Path, script: &Path, work_dir: &Path) {
    let output = Command::new(cmd)
        .current_dir(work_dir)
        .arg("/c")
        .arg(script)
        .output()
        .unwrap_or_else(|error| panic!("执行 {} 失败: {error}", cmd.display()));

    if output.status.success() {
        return;
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    panic!(
        "nvcc 编译 kernel.cu 失败, 退出码 {:?}\n--- stdout ---\n{stdout}\n--- stderr ---\n{stderr}",
        output.status.code()
    );
}

/// 取 Windows 侧的临时目录, 返回对应的 Linux 路径.
///
/// 直接读环境变量拿不到 Windows 的值, 所以问一次 cmd.exe.
/// 探测时用 `C:\` 当工作目录, 避开 UNC 问题.
fn windows_temp_dir() -> Option<PathBuf> {
    let output = Command::new("/mnt/c/Windows/System32/cmd.exe")
        .current_dir("/mnt/c")
        .args(["/c", "echo %TEMP%"])
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&output.stdout);
    let line = text.lines().find(|line| line.contains(":\\"))?.trim();
    to_wsl_path(line)
}

/// 把 `C:\Users\x\AppData\Local\Temp` 这样的路径转成 `/mnt/c/Users/x/AppData/Local/Temp`.
fn to_wsl_path(windows: &str) -> Option<PathBuf> {
    let mut chars = windows.chars();
    let drive = chars.next()?.to_ascii_lowercase();
    if !drive.is_ascii_alphabetic() || chars.next()? != ':' {
        return None;
    }
    let tail: String = chars.collect();
    Some(PathBuf::from(format!(
        "/mnt/{drive}{}",
        tail.replace('\\', "/")
    )))
}

/// 把 `/mnt/c/Users/x` 这样的 WSL 路径转回 `C:\Users\x`, 供 Windows 程序使用.
fn to_windows_path(linux: &Path) -> Option<String> {
    let text = linux.to_str()?;
    let rest = text.strip_prefix("/mnt/")?;
    let mut chars = rest.chars();
    let drive = chars.next()?.to_ascii_uppercase();
    if !drive.is_ascii_alphabetic() || chars.next()? != '/' {
        return None;
    }
    let tail: String = chars.collect();
    Some(format!("{drive}:\\{}", tail.replace('/', "\\")))
}
