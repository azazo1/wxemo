//! 搜索相关的测试辅助: 用合成密文构造样本, 不依赖真实微信数据.
//!
//! 只在测试构建里编译.

use aes::cipher::generic_array::GenericArray;
use aes::cipher::{BlockEncrypt, KeyInit};
use aes::Aes128;

use crate::crypto::{derive_key, MAX_MESSAGE};

use super::Sample;

/// 用密钥按 AES-128-CBC 加密明文 (PKCS#7 填充), 便于构造测试样本.
pub fn encrypt(key: &[u8; 16], plain: &[u8]) -> Vec<u8> {
    let padding = 16 - plain.len() % 16;
    let mut padded = plain.to_vec();
    padded.extend(std::iter::repeat_n(padding as u8, padding));

    let cipher = Aes128::new(GenericArray::from_slice(key));
    let mut previous = *key;
    let mut out = Vec::with_capacity(padded.len());
    for chunk in padded.as_chunks::<16>().0 {
        let mut block = GenericArray::clone_from_slice(chunk);
        for index in 0..16 {
            block[index] ^= previous[index];
        }
        cipher.encrypt_block(&mut block);
        out.extend_from_slice(&block);
        previous.copy_from_slice(&block);
    }
    out
}

/// 把密文包装成样本.
pub fn sample(name: &str, body: Vec<u8>) -> Sample {
    Sample {
        name: name.to_string(),
        prefix: body[..16].to_vec(),
        body,
    }
}

/// 用给定 seed 派生密钥.
pub fn key_for(seed: u64, wxid: &[u8]) -> [u8; 16] {
    let mut message = [0u8; MAX_MESSAGE];
    derive_key(seed, wxid, &mut message)
}
