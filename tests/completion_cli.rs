//! 补全只解析命令树, 不读取或创建用户配置.

use std::collections::BTreeSet;
use std::path::Path;
use std::process::Command;

fn complete(words: &[&str], directory: &Path) -> BTreeSet<String> {
    let output = Command::new(env!("CARGO_BIN_EXE_wxemo"))
        .args(["--", "wxemo"])
        .args(words)
        .env("WXEMO_COMPLETE", "fish")
        .env("WXEMO_CONFIG_DIR", directory.join("uncreated-config"))
        .current_dir(directory)
        .output()
        .unwrap();
    assert!(output.status.success(), "{:?}", output.stderr);
    assert!(output.stderr.is_empty(), "{:?}", output.stderr);
    assert!(!directory.join("uncreated-config").exists());
    String::from_utf8(output.stdout)
        .unwrap()
        .lines()
        .map(|line| line.split('\t').next().unwrap().to_owned())
        .collect()
}

#[test]
fn completion_respects_context_values_paths_and_option_terminator() {
    let scratch = Path::new(env!("CARGO_MANIFEST_DIR")).join(".tmp");
    std::fs::create_dir_all(&scratch).unwrap();
    let directory = tempfile::tempdir_in(scratch).unwrap();
    let directory = directory.path();
    std::fs::create_dir(directory.join("example-dir")).unwrap();
    std::fs::write(directory.join("example-file"), b"fixture").unwrap();

    let root = complete(&[""], directory);
    assert!(root.contains("completion"));
    assert!(!root.contains("__decode-wxgf"));
    assert!(!root.contains("--backend"));

    let seed = complete(&["--verbose", "seed", ""], directory);
    assert!(seed.contains("search") && seed.contains("import"));
    assert!(!seed.contains("decode"));

    let shells = complete(&["completion", ""], directory);
    for shell in ["bash", "zsh", "fish", "powershell", "elvish"] {
        assert!(shells.contains(shell));
    }
    assert!(!shells.contains("example-file"));

    let backends = complete(&["seed", "search", "--backend", ""], directory);
    assert_eq!(
        backends,
        ["auto", "cpu", "simd", "cuda", "metal", "wgpu"]
            .map(str::to_owned)
            .into_iter()
            .collect()
    );
    let directories = complete(
        &[
            "accounts",
            "--data-root",
            "first",
            "--data-root",
            "example-",
        ],
        directory,
    );
    assert!(directories
        .iter()
        .any(|candidate| candidate.trim_end_matches(['/', '\\']) == "example-dir"));
    assert!(!directories.contains("example-file"));
    let files = complete(&["seed", "import", "--file", "example-"], directory);
    assert!(files.contains("example-file"));
    assert!(complete(&["seed", "search", "--", "--"], directory).is_empty());
}
