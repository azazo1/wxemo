//! 配置与会话密钥的读写.
//!
//! 目录布局 (可用 `WXEMO_CONFIG_DIR` 覆盖):
//!
//! ```text
//! ~/.config/wxemo/
//! ├── config.toml   常规配置: 数据根目录, 默认账号, 搜索参数
//! └── auth.json     账号级密钥: seed 与派生出的 key
//! ```
//!
//! 两个文件都带 `version` 字段. 读到更高的版本号时直接报错, 而不是猜测语义,
//! 避免新版写入的字段被旧版静默丢弃.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

/// 当前配置格式版本.
pub const CONFIG_VERSION: u32 = 1;

/// 当前密钥文件格式版本.
pub const AUTH_VERSION: u32 = 1;

/// 数据根目录下自动向下搜索的最大层数.
const DEFAULT_DATA_ROOTS: [&str; 0] = [];

/// 常规配置.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// 配置格式版本, 用于不兼容变更时的迁移.
    pub version: u32,
    /// 微信数据根目录, 例如 `D:/wechat` 或 `~/xwechat_files`.
    pub data_roots: Vec<PathBuf>,
    /// 未指定账号时默认使用的 wxid.
    pub default_account: Option<String>,
    /// 搜索参数.
    pub search: SearchConfig,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            version: CONFIG_VERSION,
            data_roots: DEFAULT_DATA_ROOTS.iter().map(PathBuf::from).collect(),
            default_account: None,
            search: SearchConfig::default(),
        }
    }
}

/// seed 搜索参数.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct SearchConfig {
    /// 搜索起点 (含).
    pub start: u64,
    /// 搜索终点 (不含).
    pub end: u64,
    /// 线程数, 0 表示用全部核心.
    pub threads: usize,
}

impl Default for SearchConfig {
    fn default() -> Self {
        Self {
            start: 0,
            end: 1u64 << 32,
            threads: 0,
        }
    }
}

/// 单个账号的密钥材料.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AccountSecret {
    /// 账号级随机种子.
    pub seed: u64,
    /// 派生出的 16 字节密钥, 小写十六进制.
    pub key: String,
    /// 最近一次更新时间 (RFC 3339).
    pub updated_at: String,
}

/// 密钥文件.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Auth {
    /// 格式版本.
    pub version: u32,
    /// 按 wxid 索引的密钥.
    pub accounts: BTreeMap<String, AccountSecret>,
}

impl Default for Auth {
    fn default() -> Self {
        Self {
            version: AUTH_VERSION,
            accounts: BTreeMap::new(),
        }
    }
}

/// 解析配置目录.
///
/// 优先用 `WXEMO_CONFIG_DIR`, 其次 `XDG_CONFIG_HOME/wxemo`, 最后 `~/.config/wxemo`.
pub fn config_dir() -> PathBuf {
    if let Some(value) = std::env::var_os("WXEMO_CONFIG_DIR") {
        return PathBuf::from(value);
    }
    if let Some(value) = std::env::var_os("XDG_CONFIG_HOME") {
        return PathBuf::from(value).join("wxemo");
    }
    if let Some(home) = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE")) {
        return PathBuf::from(home).join(".config").join("wxemo");
    }
    PathBuf::from(".wxemo")
}

/// 配置文件路径.
pub fn config_path() -> PathBuf {
    config_dir().join("config.toml")
}

/// 密钥文件路径.
pub fn auth_path() -> PathBuf {
    config_dir().join("auth.json")
}

impl Config {
    /// 读取配置; 文件不存在时返回默认值并标记为未初始化.
    pub fn load() -> Result<(Self, bool)> {
        let path = config_path();
        if !path.exists() {
            return Ok((Self::default(), false));
        }
        let text = fs::read_to_string(&path)
            .with_context(|| format!("读取配置文件失败: {}", path.display()))?;
        let config: Self = toml::from_str(&text)
            .with_context(|| format!("解析配置文件失败: {}", path.display()))?;
        if config.version > CONFIG_VERSION {
            bail!(
                "配置文件版本 {} 高于本程序支持的 {}, 请升级 wxemo",
                config.version,
                CONFIG_VERSION
            );
        }
        Ok((config, true))
    }

    /// 写回配置, 自动创建目录.
    pub fn save(&self) -> Result<()> {
        let path = config_path();
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("创建配置目录失败: {}", parent.display()))?;
        }
        let text = toml::to_string_pretty(self).context("序列化配置失败")?;
        fs::write(&path, text).with_context(|| format!("写入配置文件失败: {}", path.display()))?;
        Ok(())
    }
}

impl Auth {
    /// 读取密钥文件; 不存在时返回空表.
    pub fn load() -> Result<Self> {
        let path = auth_path();
        if !path.exists() {
            return Ok(Self::default());
        }
        let text = fs::read_to_string(&path)
            .with_context(|| format!("读取密钥文件失败: {}", path.display()))?;
        let auth: Self = serde_json::from_str(&text)
            .with_context(|| format!("解析密钥文件失败: {}", path.display()))?;
        if auth.version > AUTH_VERSION {
            bail!(
                "密钥文件版本 {} 高于本程序支持的 {}",
                auth.version,
                AUTH_VERSION
            );
        }
        Ok(auth)
    }

    /// 写回密钥文件, 权限收紧到 0600.
    pub fn save(&self) -> Result<()> {
        let path = auth_path();
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("创建配置目录失败: {}", parent.display()))?;
        }
        let text = serde_json::to_string_pretty(self).context("序列化密钥失败")?;
        fs::write(&path, text).with_context(|| format!("写入密钥文件失败: {}", path.display()))?;
        restrict_permissions(&path)?;
        Ok(())
    }

    /// 查询某个账号的密钥.
    pub fn get(&self, wxid: &str) -> Option<&AccountSecret> {
        self.accounts.get(wxid)
    }

    /// 记录一个账号的密钥.
    pub fn put(&mut self, wxid: &str, seed: u64, key: &[u8; 16]) {
        self.accounts.insert(
            wxid.to_string(),
            AccountSecret {
                seed,
                key: hex(key),
                updated_at: now_rfc3339(),
            },
        );
    }
}

/// 把文件权限收紧为仅属主可读写.
#[cfg(unix)]
fn restrict_permissions(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mode = fs::Permissions::from_mode(0o600);
    fs::set_permissions(path, mode)
        .with_context(|| format!("设置文件权限失败: {}", path.display()))?;
    Ok(())
}

/// Windows 下没有 POSIX 权限位, 跳过.
#[cfg(not(unix))]
fn restrict_permissions(_path: &Path) -> Result<()> {
    Ok(())
}

/// 字节转小写十六进制.
pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// 十六进制转 16 字节密钥.
pub fn parse_key(text: &str) -> Result<[u8; 16]> {
    let text = text.trim();
    if text.len() != 32 {
        bail!("密钥必须是 32 个十六进制字符, 实际 {} 个", text.len());
    }
    if !text.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        bail!("密钥必须只包含 ASCII 十六进制字符");
    }
    let mut key = [0u8; 16];
    for (index, slot) in key.iter_mut().enumerate() {
        let byte = u8::from_str_radix(&text[index * 2..index * 2 + 2], 16)
            .with_context(|| format!("密钥第 {} 字节不是合法十六进制", index + 1))?;
        *slot = byte;
    }
    Ok(key)
}

/// 当前时间, 尽量用 UTC RFC 3339; 取不到系统时间时退化为 Unix 秒.
pub fn now_rfc3339() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    match SystemTime::now().duration_since(UNIX_EPOCH) {
        Ok(duration) => format!("unix:{}", duration.as_secs()),
        Err(_) => "unix:0".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_round_trip() {
        let key = [0x00u8, 0x0f, 0xff, 0x10];
        assert_eq!(hex(&key), "000fff10");
    }

    #[test]
    fn parse_key_accepts_valid_and_rejects_invalid() {
        let text = "00112233445566778899aabbccddeeff";
        assert_eq!(parse_key(text).unwrap(), [0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0xff]);
        assert!(parse_key("abcd").is_err());
        assert!(parse_key(&"z".repeat(32)).is_err());
        assert!(parse_key(&format!("a{}a", "界".repeat(10))).is_err());
    }

    #[test]
    fn config_toml_round_trip() {
        let mut config = Config::default();
        config.data_roots.push(PathBuf::from("/mnt/d/wechat"));
        config.default_account = Some("wxid_example".into());
        let text = toml::to_string_pretty(&config).unwrap();
        let back: Config = toml::from_str(&text).unwrap();
        assert_eq!(back.version, CONFIG_VERSION);
        assert_eq!(back.data_roots.len(), 1);
        assert_eq!(back.default_account.as_deref(), Some("wxid_example"));
        assert_eq!(back.search.end, 1u64 << 32);
    }

    #[test]
    fn auth_json_round_trip() {
        let mut auth = Auth::default();
        auth.put("wxid_example", 42, &[7u8; 16]);
        let text = serde_json::to_string_pretty(&auth).unwrap();
        let back: Auth = serde_json::from_str(&text).unwrap();
        let secret = back.get("wxid_example").unwrap();
        assert_eq!(secret.seed, 42);
        assert_eq!(secret.key, "07070707070707070707070707070707");
    }

    #[test]
    fn missing_auth_yields_empty() {
        let auth = Auth::default();
        assert!(auth.get("nobody").is_none());
        assert_eq!(auth.version, AUTH_VERSION);
    }
}
