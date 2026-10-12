//! 既非 aarch64 也非 x86_64 时的占位实现.
//!
//! 这两个架构之外没有 SIMD 后端, `available()` 恒为 false, 调度层会走标量路径.
//! 这里保留同名的接口, 让上层的 `#[cfg]` 可以统一.

/// 每批并行路数. 不可用时没有意义, 取 1 保持类型可用.
pub const LANES: usize = 1;

/// 报错时说明本机缺什么指令集.
pub const REQUIREMENT: &str = "aarch64 NEON 或 x86_64 AVX2";

/// 本架构没有 SIMD 后端.
pub fn available() -> bool {
    false
}

/// 不会被调用: `available()` 恒为 false, 调度层不会走到这里.
pub fn md5_prefix(_words: &[u32; 16 * LANES], _out: &mut [[u8; 16]; LANES]) {
    unreachable!("本架构没有 SIMD 后端")
}
