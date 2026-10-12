//! 按格式规范切分多图拼接容器.
//!
//! 不能用"第一个 0x3b / FFD9"判定 GIF/JPEG 的结束: 这些字节在压缩数据内部
//! 也会出现, 那样切出来的边界是错的, 还会把中间数据整段丢掉.
//! 这里按各格式的块结构逐段走:
//!
//! - PNG: 逐 chunk 走, 到 IEND 结束;
//! - GIF: 走 logical screen descriptor 与 color table, 再逐个处理
//!   0x2C 图像块 / 0x21 扩展块, 到 0x3B trailer 结束;
//! - JPEG: 走标记段, 遇到 SOS 后改为逐字节扫描熵编码数据,
//!   跳过 FF00 字节填充与 FFD0-FFD7 重启标记, 到 FFD9 结束;
//! - WebP: RIFF 长度字段.

/// 一个切分片段.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Segment {
    /// 起始偏移 (含).
    pub start: usize,
    /// 结束偏移 (开区间).
    pub end: usize,
    /// 格式名.
    pub kind: &'static str,
}

/// PNG: 逐 chunk 走到 IEND.
fn png_end(data: &[u8], start: usize) -> Option<usize> {
    if data.len() < start + 8 || &data[start..start + 8] != b"\x89PNG\r\n\x1a\n" {
        return None;
    }
    let mut pos = start + 8;
    while pos + 12 <= data.len() {
        let length = u32::from_be_bytes(data[pos..pos + 4].try_into().ok()?) as usize;
        let kind = &data[pos + 4..pos + 8];
        let end = pos.checked_add(12)?.checked_add(length)?;
        if end > data.len() {
            return None;
        }
        pos = end;
        if kind == b"IEND" {
            return (length == 0).then_some(pos);
        }
    }
    None
}

/// 跳过 GIF 的 sub-block 链, 返回下一个块的位置.
fn skip_sub_blocks(data: &[u8], mut pos: usize) -> Option<usize> {
    while pos < data.len() {
        let size = data[pos] as usize;
        pos += 1;
        if size == 0 {
            return Some(pos);
        }
        pos = pos.checked_add(size)?;
    }
    None
}

/// GIF: 按块结构走到 trailer.
fn gif_end(data: &[u8], start: usize) -> Option<usize> {
    if data.len() < start + 6 {
        return None;
    }
    let header = &data[start..start + 6];
    if header != b"GIF87a" && header != b"GIF89a" {
        return None;
    }
    let mut pos = start + 6;
    if pos + 7 > data.len() {
        return None;
    }
    let flags = data[pos + 4];
    pos += 7;
    if flags & 0x80 != 0 {
        pos += 3 * (1usize << ((flags & 0x07) + 1));
    }
    while pos < data.len() {
        match data[pos] {
            0x3B => return Some(pos + 1),
            0x2C => {
                if pos + 10 > data.len() {
                    return None;
                }
                let local_flags = data[pos + 9];
                pos += 10;
                if local_flags & 0x80 != 0 {
                    pos += 3 * (1usize << ((local_flags & 0x07) + 1));
                }
                if pos >= data.len() {
                    return None;
                }
                pos += 1;
                pos = skip_sub_blocks(data, pos)?;
            }
            0x21 => {
                pos = skip_sub_blocks(data, pos + 2)?;
            }
            _ => return None,
        }
    }
    None
}

/// 跳过 SOS 之后的熵编码数据, 停在下一个真实标记的 FF 处.
fn skip_entropy(data: &[u8], mut pos: usize) -> Option<usize> {
    while pos + 1 < data.len() {
        if data[pos] == 0xFF {
            let next = data[pos + 1];
            if next != 0x00 && !(0xD0..=0xD7).contains(&next) {
                return Some(pos);
            }
            pos += 2;
        } else {
            pos += 1;
        }
    }
    None
}

/// JPEG: 按标记段走到 EOI.
fn jpeg_end(data: &[u8], start: usize) -> Option<usize> {
    if data.len() < start + 2 || &data[start..start + 2] != b"\xff\xd8" {
        return None;
    }
    let mut pos = start + 2;
    while pos + 1 < data.len() {
        if data[pos] != 0xFF {
            return None;
        }
        // JPEG 允许标记前有多个 FF 填充字节.
        while pos < data.len() && data[pos] == 0xFF {
            pos += 1;
        }
        let marker = *data.get(pos)?;
        pos += 1;
        if marker == 0x00 || marker == 0xD8 {
            return None;
        }
        if marker == 0xD9 {
            return Some(pos);
        }
        if marker == 0x01 || (0xD0..=0xD7).contains(&marker) {
            continue;
        }
        if pos + 2 > data.len() {
            return None;
        }
        let length = u16::from_be_bytes(data[pos..pos + 2].try_into().ok()?) as usize;
        if length < 2 {
            return None;
        }
        pos = pos.checked_add(length)?;
        if pos > data.len() {
            return None;
        }
        if marker == 0xDA {
            pos = skip_entropy(data, pos)?;
        }
    }
    None
}

/// WebP: RIFF 长度字段, 注意 RIFF 长度不含自身 8 字节.
fn webp_end(data: &[u8], start: usize) -> Option<usize> {
    if data.len() < start + 12
        || &data[start..start + 4] != b"RIFF"
        || &data[start + 8..start + 12] != b"WEBP"
    {
        return None;
    }
    let size = u32::from_le_bytes(data[start + 4..start + 8].try_into().ok()?) as usize;
    let end = start.checked_add(8)?.checked_add(size)?;
    if size >= 4 && end <= data.len() {
        Some(end)
    } else {
        None
    }
}

/// 按输入顺序切分, 返回无重叠且覆盖所有字节的片段列表.
///
/// 已识别图片的内部载荷不再扫描. 未识别区域按 `bin` 保留.
/// wxgf 没有已知长度边界, 从其头部到输入末尾作为不透明片段保留.
pub fn split(data: &[u8]) -> Vec<Segment> {
    let mut segments = Vec::new();
    let mut cursor = 0;
    let mut pending = 0;
    while cursor < data.len() {
        let rest = &data[cursor..];
        let candidate = if rest.starts_with(b"\x89PNG\r\n\x1a\n") {
            png_end(data, cursor).map(|end| (end, "png"))
        } else if rest.starts_with(b"GIF8") {
            gif_end(data, cursor).map(|end| (end, "gif"))
        } else if rest.starts_with(b"\xff\xd8\xff") {
            jpeg_end(data, cursor).map(|end| (end, "jpg"))
        } else if rest.starts_with(b"RIFF") {
            webp_end(data, cursor).map(|end| (end, "webp"))
        } else if rest.starts_with(b"wxgf") {
            Some((data.len(), "wxgf"))
        } else {
            None
        };
        if let Some((end, kind)) = candidate {
            if pending < cursor {
                segments.push(Segment { start: pending, end: cursor, kind: "bin" });
            }
            segments.push(Segment { start: cursor, end, kind });
            cursor = end;
            pending = end;
        } else {
            cursor += 1;
        }
    }
    if pending < data.len() {
        segments.push(Segment { start: pending, end: data.len(), kind: "bin" });
    }
    segments
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 造一个最小的合法 PNG (一个 IDAT + IEND).
    fn tiny_png(payload: &[u8]) -> Vec<u8> {
        let mut out = b"\x89PNG\r\n\x1a\n".to_vec();
        let mut push = |kind: &[u8], body: &[u8]| {
            out.extend_from_slice(&(body.len() as u32).to_be_bytes());
            out.extend_from_slice(kind);
            out.extend_from_slice(body);
            // CRC 不参与解析, 填 0 即可
            out.extend_from_slice(&[0, 0, 0, 0]);
        };
        push(b"IHDR", &[0; 13]);
        push(b"IDAT", payload);
        push(b"IEND", &[]);
        out
    }

    /// 造一个最小的合法 GIF89a.
    fn tiny_gif() -> Vec<u8> {
        let mut out = b"GIF89a".to_vec();
        out.extend_from_slice(&[0x40, 0x00, 0x40, 0x00, 0x80, 0x00, 0x00]);
        out.extend_from_slice(&[0u8; 6]); // 2 色全局调色板
        out.push(0x2C);
        out.extend_from_slice(&[0, 0, 0, 0, 0x40, 0x00, 0x40, 0x00, 0x00]);
        out.push(0x02);
        out.extend_from_slice(&[0x02, 0x44, 0x01, 0x00]); // sub-block + 终止
        out.push(0x3B);
        out
    }

    #[test]
    fn png_segment_is_exact() {
        let png = tiny_png(&[1, 2, 3, 4]);
        assert_eq!(png_end(&png, 0), Some(png.len()));
    }

    #[test]
    fn gif_segment_is_exact() {
        let gif = tiny_gif();
        assert_eq!(gif_end(&gif, 0), Some(gif.len()));
    }

    #[test]
    fn concatenated_pngs_split_without_gaps() {
        let a = tiny_png(&[1, 2, 3]);
        let b = tiny_png(&[4, 5, 6, 7, 8]);
        let mut joined = a.clone();
        joined.extend_from_slice(&b);

        let segments = split(&joined);
        assert_eq!(segments.len(), 2);
        assert_eq!(segments[0].start, 0);
        assert_eq!(segments[0].end, a.len());
        assert_eq!(segments[1].start, a.len());
        assert_eq!(segments[1].end, joined.len());
        // 无缝铺满, 这正是旧实现在 GIF/JPEG 上做不到的
        let covered: usize = segments.iter().map(|s| s.end - s.start).sum();
        assert_eq!(covered, joined.len());
    }

    #[test]
    fn concatenated_gifs_split_without_gaps() {
        let gif = tiny_gif();
        let mut joined = gif.clone();
        joined.extend_from_slice(&gif);
        joined.extend_from_slice(&gif);

        let segments = split(&joined);
        assert_eq!(segments.len(), 3);
        let covered: usize = segments.iter().map(|s| s.end - s.start).sum();
        assert_eq!(covered, joined.len());
    }

    #[test]
    fn gif_trailer_byte_inside_data_does_not_truncate() {
        // 0x3B 出现在 sub-block 的载荷里时, 按长度跳过的解析器不会误判成 trailer.
        // 旧实现按"第一个 0x3B"扫描, 会在这里错误截断.
        let mut gif = b"GIF89a".to_vec();
        gif.extend_from_slice(&[0x40, 0x00, 0x40, 0x00, 0x80, 0x00, 0x00]);
        gif.extend_from_slice(&[0u8; 6]); // 2 色全局调色板
        gif.push(0x2C);
        gif.extend_from_slice(&[0, 0, 0, 0, 0x40, 0x00, 0x40, 0x00, 0x00]);
        gif.push(0x02);
        // 载荷 2 字节, 其中第一字节就是 0x3B
        gif.extend_from_slice(&[0x02, 0x3B, 0x44, 0x00]);
        gif.push(0x3B);

        assert_eq!(gif_end(&gif, 0), Some(gif.len()));
    }

    #[test]
    fn opaque_and_unknown_data_are_preserved() {
        assert_eq!(split(&[0u8; 64]), vec![Segment { start: 0, end: 64, kind: "bin" }]);
        let mut wxgf = b"wxgf\x13\x00\x00\x00".to_vec();
        wxgf.extend_from_slice(&tiny_png(&[1, 2, 3]));
        assert_eq!(split(&wxgf), vec![Segment { start: 0, end: wxgf.len(), kind: "wxgf" }]);
        assert!(split(&[]).is_empty());
    }

    #[test]
    fn jpeg_skips_segment_payload_and_entropy_markers() {
        // APP 内的 FFD9 不是结束标记, 熵编码中的 FF00 与重启标记也不是.
        let jpeg = b"\xff\xd8\xff\xe1\x00\x06\xff\xd9ab\xff\xda\x00\x02\x11\xff\x00\xd9\xff\xd0\x22\xff\xff\xd9";
        assert_eq!(jpeg_end(jpeg, 0), Some(jpeg.len()));
        let mut joined = jpeg.to_vec();
        joined.extend_from_slice(jpeg);
        let segments = split(&joined);
        assert_eq!(segments.len(), 2);
        assert_eq!(segments[0].end, jpeg.len());
        assert_eq!(segments[1].start, jpeg.len());
    }

    #[test]
    fn truncated_inputs_keep_every_byte() {
        let fixtures = [tiny_png(&[1, 2, 3]), tiny_gif(),
            b"\xff\xd8\xff\xe0\x00\x02\xff\xd9".to_vec(),
            b"RIFF\x04\x00\x00\x00WEBP".to_vec()];
        for fixture in fixtures {
            for length in 0..=fixture.len() {
                let body = &fixture[..length];
                let segments = split(body);
                let mut cursor = 0;
                let mut restored = Vec::new();
                for segment in segments {
                    assert_eq!(segment.start, cursor);
                    assert!(segment.end > segment.start && segment.end <= body.len());
                    restored.extend_from_slice(&body[segment.start..segment.end]);
                    cursor = segment.end;
                }
                assert_eq!(cursor, length);
                assert_eq!(restored, body);
            }
        }
        assert_eq!(webp_end(b"RIFF\x00\x00\x00\x00WEBP", 0), None);
    }
}
