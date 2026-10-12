//! `wxemo extract` 子命令: 解密并还原表情包.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use tracing::{debug, info, warn};

use crate::config::{parse_key, Auth, Config};
use crate::container;
use crate::crypto;
use crate::discover;

/// 还原参数.
pub struct ExtractOptions {
    /// 显式指定的账号 (wxid 或目录名).
    pub account: Option<String>,
    /// 直接指定表情包密文目录.
    pub emoticon_dir: Option<PathBuf>,
    /// 输出目录.
    pub out: PathBuf,
    /// 直接给定密钥 (十六进制), 优先于配置.
    pub key: Option<String>,
    /// 直接给定 seed, 与账号标识一起派生密钥.
    pub seed: Option<u64>,
    /// 额外/替代的数据根目录.
    pub data_roots: Vec<String>,
    /// 多帧容器是否拆成单独文件 (默认拆).
    pub split_frames: bool,
    /// 是否在输出目录写入清单.
    pub manifest: bool,
}

/// 执行还原.
pub fn run_extract(options: ExtractOptions, interactive: bool) -> Result<()> {
    let (config, _) = Config::load()?;
    let auth = Auth::load()?;

    // 1. 目标账号与密文目录.
    let (wxid, emoticon_dir) = resolve_target(&options, &config, interactive)?;
    info!(wxid, dir = %emoticon_dir.display(), "还原目标");

    // 2. 确定密钥.
    let key = resolve_key(&options, &auth, &wxid)?;
    debug!("解密密钥已就绪");

    // 3. 遍历解密.
    let mut files: Vec<PathBuf> = Vec::new();
    collect_files(&emoticon_dir, &mut files)?;
    files.sort();
    if files.is_empty() {
        bail!("{} 下没有文件", emoticon_dir.display());
    }
    info!(count = files.len(), "待解密文件");

    fs::create_dir_all(&options.out)
        .with_context(|| format!("创建输出目录失败: {}", options.out.display()))?;

    let mut formats: BTreeMap<String, usize> = BTreeMap::new();
    let mut entries: Vec<String> = Vec::new();
    let mut decrypted = 0usize;
    let mut written = 0usize;
    let mut bad_padding = 0usize;
    let mut bad_size = 0usize;
    let mut unrecognised = 0usize;
    let mut plaintext_bytes = 0usize;
    let mut output_bytes = 0usize;

    for path in &files {
        let blob = fs::read(path).with_context(|| format!("读取失败: {}", path.display()))?;
        if blob.is_empty() || blob.len() % 16 != 0 {
            warn!(path = %path.display(), len = blob.len(), "长度不是 16 的正整数倍, 已跳过");
            bad_size += 1;
            continue;
        }
        let Some((body, padding_ok)) = crypto::decrypt_file(&key, &blob) else {
            warn!(path = %path.display(), "解密失败, 已跳过");
            continue;
        };
        if !padding_ok {
            bad_padding += 1;
        }
        decrypted += 1;

        let rel = path
            .strip_prefix(&emoticon_dir)
            .unwrap_or(path)
            .to_path_buf();
        let pieces = build_pieces(&body, options.split_frames);
        let piece_bytes: usize = pieces.iter().map(|(chunk, _)| chunk.len()).sum();
        anyhow::ensure!(
            piece_bytes == body.len(),
            "切分字节数不匹配: {}",
            path.display()
        );
        plaintext_bytes += body.len();

        for (index, (chunk, kind)) in pieces.iter().enumerate() {
            if *kind == "bin" {
                unrecognised += 1;
            }
            *formats.entry((*kind).to_string()).or_insert(0) += 1;

            let suffix = if pieces.len() > 1 {
                format!(".{index:03}")
            } else {
                String::new()
            };
            let mut out = options.out.join(&rel);
            let name = format!(
                "{}{}.{}",
                out.file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_default(),
                suffix,
                kind
            );
            out.set_file_name(name);
            if let Some(parent) = out.parent() {
                fs::create_dir_all(parent)
                    .with_context(|| format!("创建目录失败: {}", parent.display()))?;
            }
            fs::write(&out, chunk).with_context(|| format!("写入失败: {}", out.display()))?;
            written += 1;
            output_bytes += chunk.len();
            entries.push(format!(
                "{}\t{}\t{}",
                out.strip_prefix(&options.out).unwrap_or(&out).display(),
                kind,
                chunk.len()
            ));
        }
        debug!(path = %path.display(), pieces = pieces.len(), "已处理");
    }

    // 4. 汇总.
    println!("解密文件 {decrypted} 个, 写出片段 {written} 段");
    info!(plaintext_bytes, output_bytes, "明文与输出字节统计");
    println!("格式分布: {formats:?}");
    if bad_size > 0 {
        println!("长度异常 {bad_size} 个 (已跳过)");
    }
    if bad_padding > 0 {
        println!("填充异常 {bad_padding} 个 (内容仍已写出, 请核对密钥是否正确)");
    }
    if unrecognised > 0 {
        println!("未识别格式 {unrecognised} 段 (原样写出为 .bin)");
    }
    println!("输出目录: {}", options.out.display());

    if options.manifest {
        let mut text = String::from("path\tformat\tbytes\n");
        text.push_str(&entries.join("\n"));
        text.push('\n');
        let manifest = options.out.join("manifest.tsv");
        fs::write(&manifest, text).context("写入清单失败")?;
        println!("清单: {}", manifest.display());
    }

    if bad_padding > 0 {
        warn!("存在填充异常, 密钥可能不对");
    }
    Ok(())
}

/// 把明文切成若干 (内容, 格式) 片段.
///
/// 未识别区域同样作为片段返回, 按原顺序拼接可以恢复完整明文.
fn build_pieces(body: &[u8], split_frames: bool) -> Vec<(Vec<u8>, &'static str)> {
    let segments = container::split(body);
    if !split_frames || segments.is_empty() {
        let kind = segments
            .first()
            .map(|segment| segment.kind)
            .unwrap_or("bin");
        return vec![(body.to_vec(), kind)];
    }
    segments
        .iter()
        .map(|segment| (body[segment.start..segment.end].to_vec(), segment.kind))
        .collect()
}

/// 确定账号标识与密文目录.
fn resolve_target(
    options: &ExtractOptions,
    config: &Config,
    interactive: bool,
) -> Result<(String, PathBuf)> {
    if let Some(dir) = &options.emoticon_dir {
        if !dir.is_dir() {
            bail!("表情包目录不存在: {}", dir.display());
        }
        let folder = dir
            .parent()
            .and_then(|p| p.parent())
            .and_then(|p| p.file_name())
            .map(|name| name.to_string_lossy().to_string())
            .context("无法从目录结构推断账号, 请改用 --account")?;
        return Ok((discover::normalize_wxid(&folder), dir.clone()));
    }

    let roots = discover::defaults::resolve(&options.data_roots, &config.data_roots)?;
    if roots.is_empty() {
        bail!("尚未配置数据根目录, 先运行 `wxemo config init`, 或用 --data-root 指定");
    }

    let accounts = discover::find_accounts(&roots)?;
    let account = discover::select_account(
        &accounts,
        options.account.as_deref(),
        config.default_account.as_deref(),
        interactive,
    )?;
    Ok((account.wxid.clone(), account.emoticon_dir()))
}

/// 决定使用哪个密钥: 命令行 > seed 派生 > 配置文件.
fn resolve_key(options: &ExtractOptions, auth: &Auth, wxid: &str) -> Result<[u8; 16]> {
    if let Some(text) = &options.key {
        return parse_key(text);
    }
    if let Some(seed) = options.seed {
        let mut message = [0u8; crypto::MAX_MESSAGE];
        return Ok(crypto::derive_key(seed, wxid.as_bytes(), &mut message));
    }
    if let Some(secret) = auth.get(wxid) {
        return parse_key(&secret.key);
    }
    bail!("没有 {wxid} 的密钥. 先运行 `wxemo seed search`, 或用 seed import / --key / --seed 指定")
}

/// 递归收集文件, 遍历失败时明确报错, 不静默遗漏子目录.
fn collect_files(dir: &Path, out: &mut Vec<PathBuf>) -> Result<()> {
    let entries = fs::read_dir(dir).with_context(|| format!("读取目录失败: {}", dir.display()))?;
    for entry in entries {
        let entry = entry.with_context(|| format!("读取目录项失败: {}", dir.display()))?;
        let path = entry.path();
        let kind = entry
            .file_type()
            .with_context(|| format!("读取文件类型失败: {}", path.display()))?;
        if kind.is_dir() {
            collect_files(&path, out)?;
        } else if kind.is_file() {
            out.push(path);
        } else {
            warn!(path = %path.display(), "符号链接或非普通文件, 已跳过");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tiny_png() -> Vec<u8> {
        let mut out = b"\x89PNG\r\n\x1a\n".to_vec();
        for (kind, body) in [
            (&b"IHDR"[..], &[0u8; 13][..]),
            (b"IDAT", &[1, 2, 3]),
            (b"IEND", &[]),
        ] {
            out.extend_from_slice(&(body.len() as u32).to_be_bytes());
            out.extend_from_slice(kind);
            out.extend_from_slice(body);
            out.extend_from_slice(&[0, 0, 0, 0]);
        }
        out
    }

    #[test]
    fn unknown_body_is_kept_as_single_piece() {
        // 凑满 16 字节, 让 match_magic 能识别出 wxgf
        let body = b"wxgf\x13\x00\x00\x00payload123456".to_vec();
        let pieces = build_pieces(&body, true);
        assert_eq!(pieces.len(), 1);
        assert_eq!(pieces[0].0, body);
        assert_eq!(pieces[0].1, "wxgf");
    }

    #[test]
    fn truly_unknown_body_falls_back_to_bin() {
        let body = vec![0x5au8; 64];
        let pieces = build_pieces(&body, true);
        assert_eq!(pieces.len(), 1);
        assert_eq!(pieces[0].1, "bin");
        assert_eq!(pieces[0].0.len(), 64);
    }

    #[test]
    fn concatenated_frames_split_when_requested() {
        let a = tiny_png();
        let mut joined = a.clone();
        joined.extend_from_slice(&a);
        let pieces = build_pieces(&joined, true);
        assert_eq!(pieces.len(), 2);
        // 不拆时整段保留
        let pieces = build_pieces(&joined, false);
        assert_eq!(pieces.len(), 1);
        assert_eq!(pieces[0].0.len(), joined.len());
    }

    #[test]
    fn split_preserves_prefix_gaps_and_trailer() {
        let png = tiny_png();
        let mut body = b"prefix".to_vec();
        body.extend_from_slice(&png);
        body.extend_from_slice(b"gap");
        body.extend_from_slice(&png);
        body.extend_from_slice(b"wxid_trailer");
        let pieces = build_pieces(&body, true);
        let restored: Vec<u8> = pieces
            .iter()
            .flat_map(|(chunk, _)| chunk.iter().copied())
            .collect();
        assert_eq!(restored, body);
        assert_eq!(
            pieces.iter().map(|(_, kind)| *kind).collect::<Vec<_>>(),
            ["bin", "png", "bin", "png", "bin"]
        );
        assert_eq!(build_pieces(&body, false)[0].0, body);
    }

    #[test]
    fn embedded_image_is_not_written_twice() {
        let gif = b"GIF89a\x01\x00\x01\x00\x00\x00\x00\x3b";
        let mut png = tiny_png();
        // 把一个完整 GIF 放入 PNG 的 IDAT, 不应跨格式重复提取.
        let idat = 33;
        png.splice(
            idat..idat + 15,
            [
                &(gif.len() as u32).to_be_bytes()[..],
                &b"IDAT"[..],
                &gif[..],
                &[0; 4][..],
            ]
            .concat(),
        );
        let pieces = build_pieces(&png, true);
        assert_eq!(pieces.len(), 1);
        assert_eq!(pieces[0].0, png);
        assert_eq!(pieces[0].1, "png");
    }

    #[test]
    fn key_resolution_prefers_explicit() {
        let auth = Auth::default();
        let options = ExtractOptions {
            account: None,
            emoticon_dir: None,
            out: PathBuf::from("/tmp/x"),
            key: Some("00112233445566778899aabbccddeeff".into()),
            seed: None,
            data_roots: vec![],
            split_frames: true,
            manifest: false,
        };
        assert_eq!(resolve_key(&options, &auth, "wxid_x").unwrap()[0], 0x00);
    }

    #[test]
    fn key_resolution_uses_seed() {
        let auth = Auth::default();
        let options = ExtractOptions {
            account: None,
            emoticon_dir: None,
            out: PathBuf::from("/tmp/x"),
            key: None,
            seed: Some(42),
            data_roots: vec![],
            split_frames: true,
            manifest: false,
        };
        let mut message = [0u8; crypto::MAX_MESSAGE];
        let expected = crypto::derive_key(42, b"wxid_x", &mut message);
        assert_eq!(resolve_key(&options, &auth, "wxid_x").unwrap(), expected);
    }

    #[test]
    fn key_resolution_fails_without_source() {
        let auth = Auth::default();
        let options = ExtractOptions {
            account: None,
            emoticon_dir: None,
            out: PathBuf::from("/tmp/x"),
            key: None,
            seed: None,
            data_roots: vec![],
            split_frames: true,
            manifest: false,
        };
        assert!(resolve_key(&options, &auth, "wxid_x").is_err());
    }
}
