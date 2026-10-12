//! 用合成媒体库测试完整解码入口, 不依赖真实微信安装或私人图片.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn input(marker: u8) -> Vec<u8> {
    let mut data = vec![0; 24];
    data[..4].copy_from_slice(b"wxgf");
    data[4] = 17;
    data[8] = 1;
    data[10] = 1;
    data[12] = 1;
    data[15] = 0x81;
    data[23] = marker;
    data
}

fn run(root: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_wxemo"))
        .args(args)
        .env("WXEMO_CONFIG_DIR", root.join("cfg"))
        .output()
        .unwrap()
}

#[test]
fn native_worker_keeps_animation_continues_after_failure_and_enforces_timeout() {
    let project = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let temporary = project.join(".tmp");
    fs::create_dir_all(&temporary).unwrap();
    let root = tempfile::Builder::new()
        .prefix("decode-test-")
        .tempdir_in(temporary)
        .unwrap();
    let library = root.path().join(if cfg!(windows) {
        "mock_decoder.dll"
    } else if cfg!(target_os = "macos") {
        "libmock_decoder.dylib"
    } else {
        "libmock_decoder.so"
    });
    let compiler = std::env::var_os("RUSTC").unwrap_or_else(|| "rustc".into());
    let compile = Command::new(compiler)
        .args(["--edition=2021", "--crate-type=cdylib"])
        .arg(project.join("tests/fixtures/mock_decoder.rs"))
        .arg("-o")
        .arg(&library)
        .output()
        .unwrap();
    assert!(
        compile.status.success(),
        "{}",
        String::from_utf8_lossy(&compile.stderr)
    );
    let source = root.path().join("source");
    let output = root.path().join("out");
    fs::create_dir_all(source.join("nested")).unwrap();
    fs::write(source.join("a-fail.wxgf"), input(1)).unwrap();
    fs::write(source.join("b-timeout.wxgf"), input(2)).unwrap();
    fs::write(source.join("nested/c-good.WXGF"), input(0)).unwrap();
    fs::write(source.join("ignored.txt"), b"not a WXGF").unwrap();
    let result = run(
        root.path(),
        &[
            "decode",
            source.to_str().unwrap(),
            "--out",
            output.to_str().unwrap(),
            "--decoder-lib",
            library.to_str().unwrap(),
            "--timeout-secs",
            "1",
        ],
    );
    assert!(!result.status.success());
    assert!(!output.join("a-fail.png").exists());
    assert!(!output.join("b-timeout.png").exists());
    let converted = output.join("nested/c-good.png");
    let expected = include_bytes!("fixtures/mock-animation.png");
    assert_eq!(fs::read(&converted).unwrap(), expected);
    assert_eq!(fs::read(source.join("a-fail.wxgf")).unwrap(), input(1));
    assert_eq!(fs::read(source.join("b-timeout.wxgf")).unwrap(), input(2));
    assert_eq!(
        fs::read(source.join("nested/c-good.WXGF")).unwrap(),
        input(0)
    );
    assert!(!root.path().join("cfg").exists());
    let repeated = run(
        root.path(),
        &[
            "decode",
            source.to_str().unwrap(),
            "--out",
            output.to_str().unwrap(),
            "--decoder-lib",
            library.to_str().unwrap(),
            "--timeout-secs",
            "1",
        ],
    );
    assert!(!repeated.status.success());
    assert_eq!(fs::read(converted).unwrap(), expected);
    let nested_output = source.join("inside");
    let rejected = run(
        root.path(),
        &[
            "decode",
            source.to_str().unwrap(),
            "--out",
            nested_output.to_str().unwrap(),
            "--decoder-lib",
            library.to_str().unwrap(),
        ],
    );
    assert!(!rejected.status.success());
    assert!(!nested_output.exists());
}
