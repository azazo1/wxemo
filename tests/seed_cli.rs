//! 在隔离配置目录测试 seed 子命令, 不使用真实账号数据.
use std::fs;
use std::path::PathBuf;
use std::process::{Command, Output};
use std::time::{SystemTime, UNIX_EPOCH};

use aes::cipher::{BlockEncrypt, KeyInit};
use md5::{Digest, Md5};
use serde_json::Value;

struct Fixture(PathBuf);

impl Fixture {
    fn new() -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join(".tmp")
            .join(format!("seed-test-{}-{nonce}", std::process::id()));
        fs::create_dir_all(&root).unwrap();
        Self(root)
    }

    fn run(&self, args: &[&str], success: bool) -> Output {
        let output = Command::new(env!("CARGO_BIN_EXE_wxemo"))
            .arg("--no-interactive")
            .args(args)
            .env("WXEMO_CONFIG_DIR", self.0.join("cfg"))
            .env("HOME", &self.0)
            .env("USERPROFILE", &self.0)
            .env("XDG_CONFIG_HOME", self.0.join(".config"))
            .env("XDG_DATA_HOME", self.0.join(".local/share"))
            .env("XDG_DOCUMENTS_DIR", self.0.join("XdgDocs"))
            .env_remove("OneDrive")
            .env_remove("OneDriveConsumer")
            .env_remove("OneDriveCommercial")
            .output()
            .unwrap();
        assert_eq!(
            output.status.success(),
            success,
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        output
    }

    fn records(&self) -> Value {
        serde_json::from_slice(&fs::read(self.0.join("cfg/auth.json")).unwrap()).unwrap()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let intended_parent = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(".tmp");
        assert_eq!(self.0.parent(), Some(intended_parent.as_path()));
        assert!(self
            .0
            .file_name()
            .unwrap()
            .to_string_lossy()
            .starts_with("seed-test-"));
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[test]
fn management_isolated_import_status_overwrite_and_confirmed_remove() {
    let fixture = Fixture::new();
    fixture.run(&["seed"], false);
    fixture.run(&["seed", "status"], true);
    fixture.run(
        &[
            "seed",
            "import",
            "42424242",
            "--account",
            "wxid_example_7e1e",
        ],
        true,
    );
    let first = fixture.records();
    assert_eq!(first["accounts"]["wxid_example"]["seed"], 42424242u64);
    let expected = format!("{:x}", Md5::digest(b"42424242wxid_exampleEMOTICON"));
    assert_eq!(first["accounts"]["wxid_example"]["key"], expected);
    let output = fixture.run(&["seed", "status", "--account", "wxid_example_7e1e"], true);
    for bytes in [&output.stdout, &output.stderr] {
        let text = String::from_utf8_lossy(bytes);
        assert!(!text.contains(&expected));
        assert!(!text.contains("42424242"));
    }
    fixture.run(
        &["seed", "import", "42", "--account", "wxid_example"],
        false,
    );
    assert_eq!(fixture.records(), first);
    fixture.run(
        &[
            "seed",
            "import",
            "42",
            "--account",
            "wxid_example",
            "--force",
        ],
        true,
    );
    fixture.run(&["seed", "import", "43", "--account", "wxid_other"], true);
    let before = fixture.records();
    fixture.run(&["seed", "remove", "--account", "wxid_example"], false);
    assert_eq!(fixture.records(), before);
    fixture.run(
        &["seed", "remove", "--account", "wxid_example_7e1e", "--yes"],
        true,
    );
    let after = fixture.records();
    assert!(after["accounts"].get("wxid_example").is_none());
    assert_eq!(
        after["accounts"]["wxid_other"],
        before["accounts"]["wxid_other"]
    );
    fixture.run(
        &["seed", "remove", "--account", "wxid_example", "--yes"],
        false,
    );
    assert_eq!(fixture.records(), after);
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
#[test]
fn default_discovery_works_before_configuration_and_initializes_selected_root() {
    let fixture = Fixture::new();
    #[cfg(target_os = "macos")]
    let root = fixture
        .0
        .join("Library/Containers/com.tencent.xinWeChat/Data/Documents/xwechat_files");
    #[cfg(target_os = "linux")]
    let root = fixture.0.join("XdgDocs/xwechat_files");
    fs::create_dir_all(root.join("wxid_auto_7e1e/business/emoticon")).unwrap();
    fixture.run(&["accounts"], true);
    assert!(!fixture.0.join("cfg/config.toml").exists());
    fixture.run(&["config", "init"], true);
    let text = fs::read_to_string(fixture.0.join("cfg/config.toml")).unwrap();
    let config: toml::Value = toml::from_str(&text).unwrap();
    assert_eq!(
        config["data_roots"][0].as_str().unwrap(),
        root.to_str().unwrap()
    );
}

#[test]
fn search_subcommand_saves_synthetic_seed() {
    let fixture = Fixture::new();
    let source = fixture.0.join("wxid_search_7e1e/business/emoticon");
    fs::create_dir_all(source.join("Persist/aa")).unwrap();
    let key = Md5::digest(b"42wxid_searchEMOTICON");
    let cipher = aes::Aes128::new(&key);
    let mut body = b"\x89PNG\r\n\x1a\n".to_vec();
    body.resize(64, 0);
    body.extend([16u8; 16]);
    let mut previous = key;
    let mut ciphertext = Vec::new();
    for chunk in body.as_chunks::<16>().0 {
        let mut block = aes::cipher::generic_array::GenericArray::clone_from_slice(chunk);
        for index in 0..16 {
            block[index] ^= previous[index];
        }
        cipher.encrypt_block(&mut block);
        ciphertext.extend_from_slice(&block);
        previous = block;
    }
    fs::write(source.join("Persist/aa/sample"), ciphertext).unwrap();
    fixture.run(
        &[
            "seed",
            "search",
            "--emoticon-dir",
            source.to_str().unwrap(),
            "--start",
            "40",
            "--end",
            "44",
            "--samples",
            "1",
        ],
        true,
    );
    assert_eq!(fixture.records()["accounts"]["wxid_search"]["seed"], 42u64);
}
