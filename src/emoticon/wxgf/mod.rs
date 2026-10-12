//! WXGF 头部及原生解码结果校验.

pub(crate) mod native;

use anyhow::{bail, ensure, Context, Result};

pub const MAX_INPUT_BYTES: usize = 64 * 1024 * 1024;
pub const MAX_OUTPUT_BYTES: usize = 128 * 1024 * 1024;
const PNG_SIGNATURE: &[u8; 8] = b"\x89PNG\r\n\x1a\n";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Header {
    pub width: u32,
    pub height: u32,
    pub frames: u32,
}

impl Header {
    /// 已验证布局中, 宽高和额外帧数均为大端 16 位整数.
    pub fn parse(data: &[u8]) -> Result<Self> {
        ensure!(data.len() <= MAX_INPUT_BYTES, "WXGF 文件超过 64 MiB 限制");
        ensure!(
            data.len() >= 17 && data.starts_with(b"wxgf"),
            "不是有效的 WXGF 文件"
        );
        let version = data[6];
        ensure!(
            matches!(version, 0 | 2),
            "未支持的 WXGF 布局版本: {version}"
        );
        let kind_offset = 15 + usize::from(version == 2);
        let header_len = usize::from(data[4]);
        ensure!(
            header_len >= kind_offset + 2 && header_len < data.len(),
            "WXGF 头部长度无效"
        );
        ensure!(
            matches!(data[kind_offset], 0x40 | 0x81 | 0x82),
            "未支持的 WXGF 图片类型"
        );
        let read_u16 = |offset| u16::from_be_bytes([data[offset], data[offset + 1]]);
        let header = Self {
            width: u32::from(read_u16(7)),
            height: u32::from(read_u16(9)),
            frames: u32::from(read_u16(11)) + 1,
        };
        ensure!(header.width > 0 && header.height > 0, "WXGF 图片尺寸为零");
        // 限制私有解码器的工作量, 避免小文件声明异常大的解码图像.
        let total_pixels =
            u64::from(header.width) * u64::from(header.height) * u64::from(header.frames);
        ensure!(
            total_pixels <= 256 * 1024 * 1024,
            "WXGF 总像素数超过安全限制"
        );
        Ok(header)
    }
}

#[derive(Debug)]
pub struct ImageInfo {
    pub width: u32,
    pub height: u32,
    pub frames: u32,
    pub plays: Option<u32>,
    pub duration_ms: f64,
}

fn u32_at(data: &[u8], offset: usize) -> u32 {
    u32::from_be_bytes(data[offset..offset + 4].try_into().expect("字段长度已校验"))
}

/// 校验 CRC, APNG 序号, 尺寸与完整帧数, 去掉媒体库在 IEND 后附加的私有尾注.
/// 这里只做容器校验; 像素解码由媒体库负责.
pub fn validate_png(data: &mut Vec<u8>, source: Header) -> Result<ImageInfo> {
    ensure!(
        data.len() <= MAX_OUTPUT_BYTES && data.starts_with(PNG_SIGNATURE),
        "解码器没有输出有效 PNG/APNG"
    );
    let mut offset = 8usize;
    let mut dimensions = None;
    let mut animation = None;
    let mut frame_count = 0u32;
    let mut sequence = 0u32;
    let mut payload = false;
    let mut frame_payload = false;
    let mut duration_ms = 0.0;
    while offset < data.len() {
        let header_end = offset.checked_add(8).context("PNG 长度溢出")?;
        ensure!(header_end <= data.len(), "PNG chunk 头部截断");
        let length = u32_at(data, offset) as usize;
        let crc_offset = header_end
            .checked_add(length)
            .context("PNG chunk 长度溢出")?;
        let end = crc_offset.checked_add(4).context("PNG chunk 长度溢出")?;
        ensure!(end <= data.len(), "PNG chunk 数据截断");
        let tag = &data[offset + 4..header_end];
        let body = &data[header_end..crc_offset];
        ensure!(
            crc32fast::hash(&data[offset + 4..crc_offset]) == u32_at(data, crc_offset),
            "PNG chunk CRC 不匹配"
        );
        if dimensions.is_none() {
            ensure!(tag == b"IHDR", "PNG 缺少首个 IHDR");
        }
        match tag {
            b"IHDR" => {
                ensure!(dimensions.is_none() && length == 13, "PNG IHDR 无效");
                let width = u32_at(body, 0);
                let height = u32_at(body, 4);
                ensure!(
                    width == source.width && height == source.height,
                    "解码图片尺寸与 WXGF 不一致"
                );
                ensure!(
                    body[10] == 0 && body[11] == 0 && body[12] <= 1,
                    "PNG 编码字段无效"
                );
                dimensions = Some((width, height));
            }
            b"acTL" => {
                ensure!(
                    animation.is_none() && !payload && length == 8,
                    "APNG acTL 无效"
                );
                let frames = u32_at(body, 0);
                ensure!(
                    frames == source.frames,
                    "APNG 帧数与 WXGF 不一致, 拒绝首帧或不完整输出"
                );
                animation = Some((frames, u32_at(body, 4)));
            }
            b"fcTL" => {
                ensure!(animation.is_some() && length == 26, "APNG fcTL 无效");
                ensure!(frame_count == 0 || frame_payload, "APNG 帧缺少图像数据");
                ensure!(u32_at(body, 0) == sequence, "APNG 帧序号不连续");
                sequence = sequence.checked_add(1).context("APNG 序号溢出")?;
                let width = u32_at(body, 4);
                let height = u32_at(body, 8);
                ensure!(width > 0 && height > 0, "APNG 帧尺寸为零");
                ensure!(
                    u32_at(body, 12)
                        .checked_add(width)
                        .is_some_and(|end| end <= source.width)
                        && u32_at(body, 16)
                            .checked_add(height)
                            .is_some_and(|end| end <= source.height),
                    "APNG 帧超出画布"
                );
                let numerator = u16::from_be_bytes([body[20], body[21]]);
                let denominator = u16::from_be_bytes([body[22], body[23]]);
                duration_ms += f64::from(numerator) * 1000.0
                    / f64::from(if denominator == 0 { 100 } else { denominator });
                ensure!(body[24] <= 2 && body[25] <= 1, "APNG 帧合成字段无效");
                frame_count += 1;
                frame_payload = false;
            }
            b"IDAT" => {
                ensure!(frame_count <= 1, "PNG IDAT 位置无效");
                ensure!(
                    animation.is_none() || frame_count == 1,
                    "APNG 缺少第一帧控制信息"
                );
                // PNG 允许空数据分块, 但整帧必须至少包含一个非空载荷.
                payload |= length > 0;
                frame_payload |= length > 0;
            }
            b"fdAT" => {
                ensure!(
                    animation.is_some() && frame_count > 1 && length >= 4,
                    "APNG fdAT 无效"
                );
                ensure!(u32_at(body, 0) == sequence, "APNG 图像数据序号不连续");
                sequence = sequence.checked_add(1).context("APNG 序号溢出")?;
                frame_payload |= length > 4;
            }
            b"IEND" => {
                ensure!(
                    length == 0 && payload && frame_payload,
                    "PNG 结束块或图像数据无效"
                );
                let (frames, plays) = if let Some((declared, plays)) = animation {
                    ensure!(frame_count == declared, "APNG 实际帧数与声明不一致");
                    (frame_count, Some(plays))
                } else {
                    ensure!(source.frames == 1, "解码器仅输出了动画首帧");
                    (1, None)
                };
                let (width, height) = dimensions.context("PNG 缺少尺寸")?;
                data.truncate(end);
                return Ok(ImageInfo {
                    width,
                    height,
                    frames,
                    plays,
                    duration_ms,
                });
            }
            _ => {}
        }
        offset = end;
    }
    bail!("PNG 缺少 IEND 或动画数据截断")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chunk(data: &mut Vec<u8>, kind: &[u8; 4], body: &[u8]) {
        data.extend_from_slice(&(body.len() as u32).to_be_bytes());
        data.extend_from_slice(kind);
        data.extend_from_slice(body);
        data.extend_from_slice(
            &crc32fast::hash(&data[data.len() - body.len() - 4..]).to_be_bytes(),
        );
    }

    fn png(animated: bool) -> Vec<u8> {
        let mut data = PNG_SIGNATURE.to_vec();
        chunk(&mut data, b"IHDR", &[0, 0, 0, 1, 0, 0, 0, 1, 8, 6, 0, 0, 0]);
        if animated {
            chunk(&mut data, b"acTL", &[0, 0, 0, 2, 0, 0, 0, 3]);
            chunk(
                &mut data,
                b"fcTL",
                &[
                    0, 0, 0, 0, 0, 0, 0, 1, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 25, 3, 232, 0, 0,
                ],
            );
        }
        chunk(&mut data, b"IDAT", &[1]);
        if animated {
            chunk(
                &mut data,
                b"fcTL",
                &[
                    0, 0, 0, 1, 0, 0, 0, 1, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 50, 3, 232, 0, 0,
                ],
            );
            chunk(&mut data, b"fdAT", &[0, 0, 0, 2, 1]);
        }
        chunk(&mut data, b"IEND", &[]);
        data
    }

    #[test]
    fn header_versions_and_limits() {
        let mut data = vec![0; 24];
        data[..4].copy_from_slice(b"wxgf");
        data[4] = 17;
        data[8] = 193;
        data[10] = 60;
        data[11..13].copy_from_slice(&536u16.to_be_bytes());
        data[15] = 0x81;
        assert_eq!(
            Header::parse(&data).unwrap(),
            Header {
                width: 193,
                height: 60,
                frames: 537
            }
        );
        data[6] = 2;
        assert!(Header::parse(&data).is_err());
        data[4] = 18;
        data[16] = 0x81;
        assert_eq!(Header::parse(&data).unwrap().frames, 537);
        for length in 0..18 {
            assert!(Header::parse(&data[..length]).is_err());
        }
        data[6] = 1;
        assert!(Header::parse(&data).is_err());
        data[6] = 2;
        data[7..13].fill(255);
        assert!(Header::parse(&data).is_err());
    }

    #[test]
    fn png_keeps_complete_frames_timing_and_strips_footer() {
        let mut data = png(true);
        let length = data.len();
        data.extend_from_slice(b"private footer");
        let info = validate_png(
            &mut data,
            Header {
                width: 1,
                height: 1,
                frames: 2,
            },
        )
        .unwrap();
        assert_eq!(data.len(), length);
        assert_eq!(info.frames, 2);
        assert_eq!(info.plays, Some(3));
        assert_eq!(info.duration_ms, 75.0);
        let mut still = png(false);
        assert!(validate_png(
            &mut still,
            Header {
                width: 1,
                height: 1,
                frames: 2
            }
        )
        .is_err());
    }

    #[test]
    fn empty_data_chunk_is_legal_but_keeps_sequence_checks() {
        let source = Header {
            width: 1,
            height: 1,
            frames: 2,
        };
        let mut valid = png(true);
        valid.truncate(valid.len() - 12);
        let mut invalid = valid.clone();
        chunk(&mut valid, b"fdAT", &[0, 0, 0, 3]);
        chunk(&mut valid, b"IEND", &[]);
        assert_eq!(validate_png(&mut valid, source).unwrap().frames, 2);
        chunk(&mut invalid, b"fdAT", &[0, 0, 0, 4]);
        chunk(&mut invalid, b"IEND", &[]);
        assert!(validate_png(&mut invalid, source).is_err());
    }

    #[test]
    fn damaged_png_is_rejected() {
        let source = Header {
            width: 1,
            height: 1,
            frames: 2,
        };
        let original = png(true);
        for length in 0..original.len() {
            assert!(validate_png(&mut original[..length].to_vec(), source).is_err());
        }
        let mut corrupt = original.clone();
        corrupt[29] ^= 1;
        assert!(validate_png(&mut corrupt, source).is_err());
        assert!(validate_png(
            &mut original.clone(),
            Header {
                frames: 1,
                ..source
            }
        )
        .is_err());
    }
}
