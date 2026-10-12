//! 测试用的合成样本构造.
//!
//! 只在测试编译单元里存在, 不参与正式构建.

use aes::cipher::BlockEncrypt;
use aes::cipher::KeyInit;

use super::Sample;

/// 用密钥加密明文 (PKCS#7 填充), 便于构造测试样本.
pub(crate) fn encrypt(key: &[u8; 16], plain: &[u8]) -> Vec<u8> {
    let padding = 16 - plain.len() % 16;
    let mut padded = plain.to_vec();
    padded.extend(std::iter::repeat_n(padding as u8, padding));

    let cipher = aes::Aes128::new(aes::cipher::generic_array::GenericArray::from_slice(key));
    let mut previous = *key;
    let mut out = Vec::with_capacity(padded.len());
    for chunk in padded.as_chunks::<16>().0 {
        let mut block = aes::cipher::generic_array::GenericArray::clone_from_slice(chunk);
        for index in 0..16 {
            block[index] ^= previous[index];
        }
        cipher.encrypt_block(&mut block);
        out.extend_from_slice(&block);
        previous.copy_from_slice(&block);
    }
    out
}

/// 用密文整体构造一个样本, 首块取前 16 字节.
pub(crate) fn sample(name: &str, body: Vec<u8>) -> Sample {
    Sample {
        name: name.to_string(),
        prefix: body[..16].to_vec(),
        body,
    }
}

/// 三个不同格式的样本, 全部用同一个密钥加密.
pub(crate) fn three_formats(key: &[u8; 16]) -> Vec<Sample> {
    vec![
        sample("a.png", encrypt(key, b"\x89PNG\r\n\x1a\n0123456789abcdef")),
        sample("b.gif", encrypt(key, b"GIF89a0123456789abcdef")),
        sample("c.jpg", encrypt(key, b"\xff\xd8\xff\xe00123456789abcdef")),
    ]
}
