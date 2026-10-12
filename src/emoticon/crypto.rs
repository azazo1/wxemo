//! 密钥派生, 解密与格式识别.
//!
//! 公式来自对 Weixin.dll 的静态分析:
//!
//! ```text
//! key       = MD5(f"{seed}{wxid}EMOTICON").digest()[:16]
//! plaintext = AES-128-CBC(key, iv=key).decrypt(ciphertext)   # PKCS#7 填充
//! ```

use aes::cipher::generic_array::GenericArray;
use aes::cipher::{BlockDecrypt, KeyInit};
use aes::Aes128;
use md5::{Digest, Md5};

/// 拼接在消息末尾的固定后缀, 源码中以 8 字节立即数内联.
const SUFFIX: &[u8] = b"EMOTICON";

/// 候选消息缓冲长度: 20 位十进制 + 账号标识 + 后缀, 留足余量.
pub const MAX_MESSAGE: usize = 20 + 128 + 8;

/// 把 seed 的十进制形式 + 账号标识 + 后缀依次写入 `out`, 返回总长度.
pub fn build_message(out: &mut [u8], seed: u64, wxid: &[u8]) -> usize {
    let mut len = write_decimal(out, seed);
    out[len..len + wxid.len()].copy_from_slice(wxid);
    len += wxid.len();
    out[len..len + SUFFIX.len()].copy_from_slice(SUFFIX);
    len + SUFFIX.len()
}

/// 写入十进制表示, 返回位数. 0 写一位 `0`, 与二进制的 do-while 一致.
fn write_decimal(out: &mut [u8], value: u64) -> usize {
    if value == 0 {
        out[0] = b'0';
        return 1;
    }
    let mut digits = [0u8; 20];
    let mut count = 0;
    let mut rest = value;
    while rest > 0 {
        digits[count] = b'0' + (rest % 10) as u8;
        rest /= 10;
        count += 1;
    }
    for index in 0..count {
        out[index] = digits[count - 1 - index];
    }
    count
}

/// 派生候选密钥: MD5 摘要的前 16 字节.
pub fn derive_key(seed: u64, wxid: &[u8], message: &mut [u8]) -> [u8; 16] {
    let len = build_message(message, seed, wxid);
    let digest = Md5::digest(&message[..len]);
    let mut key = [0u8; 16];
    key.copy_from_slice(&digest[..16]);
    key
}

/// 构造 AES-128 解密器. 密钥编排只做一次, 供同一候选的多个样本复用.
pub fn new_cipher(key: &[u8; 16]) -> Aes128 {
    Aes128::new(GenericArray::from_slice(key))
}

/// 解出密文首块. CBC 首块需要与 IV 异或, 这里的 IV 就是密钥本身.
pub fn decrypt_first_block(cipher: &Aes128, iv: &[u8; 16], ciphertext: &[u8]) -> [u8; 16] {
    let mut block = *GenericArray::from_slice(&ciphertext[..16]);
    cipher.decrypt_block(&mut block);
    let mut out = [0u8; 16];
    for index in 0..16 {
        out[index] = block[index] ^ iv[index];
    }
    out
}

/// 解出密文末块, 需要它前面那一块密文参与 CBC 异或.
#[cfg_attr(not(test), allow(dead_code))]
pub fn decrypt_last_block(cipher: &Aes128, last: &[u8], previous: &[u8]) -> [u8; 16] {
    let mut block = *GenericArray::from_slice(&last[..16]);
    cipher.decrypt_block(&mut block);
    let mut out = [0u8; 16];
    for index in 0..16 {
        out[index] = block[index] ^ previous[index];
    }
    out
}

/// CBC 解密整个缓冲区. `iv` 通常等于密钥.
pub fn decrypt_all(cipher: &Aes128, iv: &[u8; 16], ciphertext: &[u8], out: &mut [u8]) {
    let mut previous = *iv;
    for (index, chunk) in ciphertext.as_chunks::<16>().0.iter().enumerate() {
        let mut block = *GenericArray::from_slice(chunk);
        cipher.decrypt_block(&mut block);
        for offset in 0..16 {
            out[index * 16 + offset] = block[offset] ^ previous[offset];
        }
        previous.copy_from_slice(chunk);
    }
}

/// 判定解密后的结尾是否构成合法的 PKCS#7 填充.
pub fn valid_padding(plain: &[u8]) -> bool {
    let Some(&last) = plain.last() else {
        return false;
    };
    let padding = last as usize;
    if padding == 0 || padding > 16 || padding > plain.len() {
        return false;
    }
    plain[plain.len() - padding..]
        .iter()
        .all(|byte| *byte as usize == padding)
}

/// 去掉 PKCS#7 填充; 填充非法时原样返回并报告 false.
pub fn strip_padding(data: &[u8]) -> (&[u8], bool) {
    if valid_padding(data) {
        let padding = data[data.len() - 1] as usize;
        (&data[..data.len() - padding], true)
    } else {
        (data, false)
    }
}

/// 明文首块可识别的图片格式及其魔数字节数.
///
/// 返回匹配强度 (字节数), 越强越能排除误报.
pub fn match_magic(block: &[u8]) -> Option<(&'static str, usize)> {
    if block.len() < 16 {
        return None;
    }
    const FIXED: &[(&str, &[u8])] = &[
        ("png", b"\x89PNG\r\n\x1a\n"),
        ("gif", b"GIF87a"),
        ("gif", b"GIF89a"),
        ("jpg", b"\xff\xd8\xff"),
        ("wxgf", b"wxgf"),
    ];
    for (name, magic) in FIXED {
        if block.starts_with(magic) {
            return Some((name, magic.len()));
        }
    }
    if &block[0..4] == b"RIFF" && &block[8..12] == b"WEBP" {
        return Some(("webp", 12));
    }
    None
}

/// 用密钥解开整个文件, 返回去填充后的明文与填充是否合法.
pub fn decrypt_file(key: &[u8; 16], ciphertext: &[u8]) -> Option<(Vec<u8>, bool)> {
    if ciphertext.is_empty() || !ciphertext.len().is_multiple_of(16) {
        return None;
    }
    let cipher = new_cipher(key);
    let mut plain = vec![0u8; ciphertext.len()];
    decrypt_all(&cipher, key, ciphertext, &mut plain);
    let (body, ok) = strip_padding(&plain);
    Some((body.to_vec(), ok))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn md5_matches_known_vector() {
        assert_eq!(
            format!("{:x}", Md5::digest(b"abc")),
            "900150983cd24fb0d6963f7d28e17f72"
        );
    }

    #[test]
    fn message_matches_format_macro() {
        let mut buffer = [0u8; MAX_MESSAGE];
        for seed in [0u64, 7, 123456789, 4294967295] {
            let len = build_message(&mut buffer, seed, b"wxid_example123456");
            let expected = format!("{seed}wxid_example123456EMOTICON");
            assert_eq!(&buffer[..len], expected.as_bytes());
        }
    }

    #[test]
    fn cbc_round_trip_and_last_block() {
        use aes::cipher::BlockEncrypt;

        let key = [0x11u8; 16];
        let mut plain = [0u8; 48];
        for (index, byte) in plain.iter_mut().enumerate() {
            *byte = index as u8;
        }
        let encryptor = Aes128::new(GenericArray::from_slice(&key));
        let mut previous = key;
        let mut ciphertext = [0u8; 48];
        for index in 0..3 {
            let mut block = *GenericArray::from_slice(&plain[index * 16..index * 16 + 16]);
            for offset in 0..16 {
                block[offset] ^= previous[offset];
            }
            encryptor.encrypt_block(&mut block);
            ciphertext[index * 16..index * 16 + 16].copy_from_slice(&block);
            previous.copy_from_slice(&block);
        }

        let decryptor = new_cipher(&key);
        let mut recovered = [0u8; 48];
        decrypt_all(&decryptor, &key, &ciphertext, &mut recovered);
        assert_eq!(recovered, plain);

        let last = decrypt_last_block(&decryptor, &ciphertext[32..], &ciphertext[16..32]);
        assert_eq!(last, plain[32..]);

        let first = decrypt_first_block(&decryptor, &key, &ciphertext);
        assert_eq!(first, plain[..16]);
    }

    #[test]
    fn padding_detects_bad_tail() {
        assert!(valid_padding(&[1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 3, 3, 3]));
        assert!(!valid_padding(&[1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 3, 3, 4]));
        assert!(!valid_padding(&[0u8; 16]));
    }

    #[test]
    fn strip_padding_trims_and_reports() {
        let (body, ok) = strip_padding(&[1, 2, 3, 2, 2]);
        assert!(ok);
        assert_eq!(body, &[1, 2, 3]);
        let (body, ok) = strip_padding(&[1, 2, 3, 9, 2]);
        assert!(!ok);
        assert_eq!(body, &[1, 2, 3, 9, 2]);
    }

    #[test]
    fn magic_recognises_known_formats() {
        // match_magic 只看 16 字节的块, 测试数据必须凑满 16 字节.
        assert_eq!(match_magic(b"\x89PNG\r\n\x1a\n01234567").unwrap().0, "png");
        assert_eq!(match_magic(b"GIF89a0123456789").unwrap().0, "gif");
        assert_eq!(match_magic(b"\xff\xd8\xff\xe0012345678901").unwrap().0, "jpg");
        assert_eq!(match_magic(b"wxgf\x13\x00\x00\x00012345678").unwrap().0, "wxgf");
        assert_eq!(match_magic(b"RIFF\x24\x00\x00\x00WEBPVP8X").unwrap().0, "webp");
        assert!(match_magic(b"\x00\x01\x02\x03\x04\x05\x06\x07\x08\x09\x0a\x0b\x0c\x0d\x0e\x0f").is_none());
        // 不足 16 字节一律判为不识别, 避免越界比较
        assert!(match_magic(b"\x89PNG").is_none());
    }
}
