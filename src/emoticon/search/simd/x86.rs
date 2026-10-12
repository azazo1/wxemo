//! x86_64 AVX2 后端: 8 路并行 MD5.
//!
//! AVX2 不是 x86_64 的基线特性, 因此运行期用 `is_x86_feature_detected!` 检测,
//! 只有检测通过才会进入这里.
//!
//! 这里的 intrinsic 都带 `#[target_feature(enable = "avx2")]`, 只能从同样开启了
//! AVX2 的函数里调用. 为此整段压缩函数都写在 `md5_prefix_avx2` 内部: 一旦把某一步
//! 拆成普通的 `#[inline(always)]` 辅助函数, LLVM 就会拒绝把它内联进 AVX2 上下文,
//! 性能会明显下降.

use std::arch::x86_64::*;

/// 每批并行路数.
pub const LANES: usize = 8;

/// 报错时说明本机缺什么指令集.
pub const REQUIREMENT: &str = "x86_64 AVX2";

/// 运行期检测 AVX2 (含操作系统对 YMM 状态的保存支持).
pub fn available() -> bool {
    std::arch::is_x86_feature_detected!("avx2")
}

/// MD5 的 4 个初始状态字.
const INIT: [u32; 4] = [0x6745_2301, 0xefcd_ab89, 0x98ba_dcfe, 0x1032_5476];

/// MD5 轮常量 K[i] = floor(2^32 * abs(sin(i + 1))).
const K: [u32; 64] = [
    0xd76aa478, 0xe8c7b756, 0x242070db, 0xc1bdceee, 0xf57c0faf, 0x4787c62a, 0xa8304613, 0xfd469501,
    0x698098d8, 0x8b44f7af, 0xffff5bb1, 0x895cd7be, 0x6b901122, 0xfd987193, 0xa679438e, 0x49b40821,
    0xf61e2562, 0xc040b340, 0x265e5a51, 0xe9b6c7aa, 0xd62f105d, 0x02441453, 0xd8a1e681, 0xe7d3fbc8,
    0x21e1cde6, 0xc33707d6, 0xf4d50d87, 0x455a14ed, 0xa9e3e905, 0xfcefa3f8, 0x676f02d9, 0x8d2a4c8a,
    0xfffa3942, 0x8771f681, 0x6d9d6122, 0xfde5380c, 0xa4beea44, 0x4bdecfa9, 0xf6bb4b60, 0xbebfbc70,
    0x289b7ec6, 0xeaa127fa, 0xd4ef3085, 0x04881d05, 0xd9d4d039, 0xe6db99e5, 0x1fa27cf8, 0xc4ac5665,
    0xf4292244, 0x432aff97, 0xab9423a7, 0xfc93a039, 0x655b59c3, 0x8f0ccc92, 0xffeff47d, 0x85845dd1,
    0x6fa87e4f, 0xfe2ce6e0, 0xa3014314, 0x4e0811a1, 0xf7537e82, 0xbd3af235, 0x2ad7d2bb, 0xeb86d391,
];

/// 8 路 32 位向量.
type V = __m256i;

/// 把轮常量预广播成向量.
///
/// 这样每步的常量加法可以直接用内存操作数, 不必在每批里重复广播 64 次.
#[target_feature(enable = "avx2")]
unsafe fn constants() -> &'static [V; 64] {
    static CACHE: std::sync::OnceLock<[V; 64]> = std::sync::OnceLock::new();
    CACHE.get_or_init(|| {
        let mut out = [_mm256_setzero_si256(); 64];
        for (slot, value) in out.iter_mut().zip(K) {
            // 安全: 本函数已开启 AVX2.
            *slot = _mm256_set1_epi32(value as i32);
        }
        out
    })
}

/// 广播一个标量到 8 路.
#[target_feature(enable = "avx2")]
unsafe fn set1(value: u32) -> V {
    _mm256_set1_epi32(value as i32)
}

/// 向量加法.
macro_rules! add {
    ($left:expr, $right:expr) => {
        _mm256_add_epi32($left, $right)
    };
}

/// 循环左移. 移位数必须是字面量, 才能作为 const 泛型参数.
macro_rules! rotl {
    ($value:expr, $shift:literal) => {
        _mm256_or_si256(
            _mm256_slli_epi32::<$shift>($value),
            _mm256_srli_epi32::<{ 32 - $shift }>($value),
        )
    };
}

/// 第 1 轮函数 F(b, c, d) = (b & c) | (!b & d).
macro_rules! f {
    ($b:expr, $c:expr, $d:expr) => {
        _mm256_or_si256(_mm256_and_si256($b, $c), _mm256_andnot_si256($b, $d))
    };
}

/// 第 2 轮函数 G(b, c, d) = (b & d) | (c & !d).
macro_rules! g {
    ($b:expr, $c:expr, $d:expr) => {
        _mm256_or_si256(_mm256_and_si256($b, $d), _mm256_andnot_si256($d, $c))
    };
}

/// 第 3 轮函数 H(b, c, d) = b ^ c ^ d.
macro_rules! h {
    ($b:expr, $c:expr, $d:expr) => {
        _mm256_xor_si256(_mm256_xor_si256($b, $c), $d)
    };
}

/// 第 4 轮函数 I(b, c, d) = c ^ (b | !d).
macro_rules! i {
    ($b:expr, $c:expr, $d:expr) => {
        _mm256_xor_si256(
            $c,
            _mm256_or_si256($b, _mm256_xor_si256($d, _mm256_set1_epi32(-1))),
        )
    };
}

/// 一条 MD5 步骤, 复用公共的展开顺序.
macro_rules! step {
    ($func:ident, $dst:ident, $x:ident, $y:ident, $z:ident, $k:expr, $w:expr, $s:literal) => {
        md5_step!(add, rotl, $func, $dst, $x, $y, $z, $k, $w, $s);
    };
}

/// 计算一批候选的 MD5 摘要前 16 字节.
///
/// `words` 是 16 个消息字的 SoA 布局, 第 `t` 个字的第 `lane` 路在 `words[t * LANES + lane]`.
pub fn md5_prefix(words: &[u32; 16 * LANES], out: &mut [[u8; 16]; LANES]) {
    // 调用方通过 `available()` 保证 AVX2 可用.
    debug_assert!(available());
    // 安全: 上面已确认本机支持 AVX2.
    unsafe { md5_prefix_avx2(words, out) }
}

/// AVX2 版本的压缩函数.
///
/// # Safety
///
/// 调用者必须确认本机支持 AVX2.
#[target_feature(enable = "avx2")]
unsafe fn md5_prefix_avx2(words: &[u32; 16 * LANES], out: &mut [[u8; 16]; LANES]) {
    let k = constants();

    let mut w = [_mm256_setzero_si256(); 16];
    for (index, slot) in w.iter_mut().enumerate() {
        *slot = _mm256_loadu_si256(words.as_ptr().add(index * LANES).cast());
    }

    let mut a = set1(INIT[0]);
    let mut b = set1(INIT[1]);
    let mut c = set1(INIT[2]);
    let mut d = set1(INIT[3]);

    // 第 1 轮
    step!(f, a, b, c, d, k[0], w[0], 7);
    step!(f, d, a, b, c, k[1], w[1], 12);
    step!(f, c, d, a, b, k[2], w[2], 17);
    step!(f, b, c, d, a, k[3], w[3], 22);
    step!(f, a, b, c, d, k[4], w[4], 7);
    step!(f, d, a, b, c, k[5], w[5], 12);
    step!(f, c, d, a, b, k[6], w[6], 17);
    step!(f, b, c, d, a, k[7], w[7], 22);
    step!(f, a, b, c, d, k[8], w[8], 7);
    step!(f, d, a, b, c, k[9], w[9], 12);
    step!(f, c, d, a, b, k[10], w[10], 17);
    step!(f, b, c, d, a, k[11], w[11], 22);
    step!(f, a, b, c, d, k[12], w[12], 7);
    step!(f, d, a, b, c, k[13], w[13], 12);
    step!(f, c, d, a, b, k[14], w[14], 17);
    step!(f, b, c, d, a, k[15], w[15], 22);
    // 第 2 轮
    step!(g, a, b, c, d, k[16], w[1], 5);
    step!(g, d, a, b, c, k[17], w[6], 9);
    step!(g, c, d, a, b, k[18], w[11], 14);
    step!(g, b, c, d, a, k[19], w[0], 20);
    step!(g, a, b, c, d, k[20], w[5], 5);
    step!(g, d, a, b, c, k[21], w[10], 9);
    step!(g, c, d, a, b, k[22], w[15], 14);
    step!(g, b, c, d, a, k[23], w[4], 20);
    step!(g, a, b, c, d, k[24], w[9], 5);
    step!(g, d, a, b, c, k[25], w[14], 9);
    step!(g, c, d, a, b, k[26], w[3], 14);
    step!(g, b, c, d, a, k[27], w[8], 20);
    step!(g, a, b, c, d, k[28], w[13], 5);
    step!(g, d, a, b, c, k[29], w[2], 9);
    step!(g, c, d, a, b, k[30], w[7], 14);
    step!(g, b, c, d, a, k[31], w[12], 20);
    // 第 3 轮
    step!(h, a, b, c, d, k[32], w[5], 4);
    step!(h, d, a, b, c, k[33], w[8], 11);
    step!(h, c, d, a, b, k[34], w[11], 16);
    step!(h, b, c, d, a, k[35], w[14], 23);
    step!(h, a, b, c, d, k[36], w[1], 4);
    step!(h, d, a, b, c, k[37], w[4], 11);
    step!(h, c, d, a, b, k[38], w[7], 16);
    step!(h, b, c, d, a, k[39], w[10], 23);
    step!(h, a, b, c, d, k[40], w[13], 4);
    step!(h, d, a, b, c, k[41], w[0], 11);
    step!(h, c, d, a, b, k[42], w[3], 16);
    step!(h, b, c, d, a, k[43], w[6], 23);
    step!(h, a, b, c, d, k[44], w[9], 4);
    step!(h, d, a, b, c, k[45], w[12], 11);
    step!(h, c, d, a, b, k[46], w[15], 16);
    step!(h, b, c, d, a, k[47], w[2], 23);
    // 第 4 轮
    step!(i, a, b, c, d, k[48], w[0], 6);
    step!(i, d, a, b, c, k[49], w[7], 10);
    step!(i, c, d, a, b, k[50], w[14], 15);
    step!(i, b, c, d, a, k[51], w[5], 21);
    step!(i, a, b, c, d, k[52], w[12], 6);
    step!(i, d, a, b, c, k[53], w[3], 10);
    step!(i, c, d, a, b, k[54], w[10], 15);
    step!(i, b, c, d, a, k[55], w[1], 21);
    step!(i, a, b, c, d, k[56], w[8], 6);
    step!(i, d, a, b, c, k[57], w[15], 10);
    step!(i, c, d, a, b, k[58], w[6], 15);
    step!(i, b, c, d, a, k[59], w[13], 21);
    step!(i, a, b, c, d, k[60], w[4], 6);
    step!(i, d, a, b, c, k[61], w[11], 10);
    step!(i, c, d, a, b, k[62], w[2], 15);
    step!(i, b, c, d, a, k[63], w[9], 21);

    // 密钥取摘要前 16 字节, 正好是 A, B, C, D 四个状态字.
    let mut lanes = [[0u32; LANES]; 4];
    _mm256_storeu_si256(lanes[0].as_mut_ptr().cast(), add!(a, set1(INIT[0])));
    _mm256_storeu_si256(lanes[1].as_mut_ptr().cast(), add!(b, set1(INIT[1])));
    _mm256_storeu_si256(lanes[2].as_mut_ptr().cast(), add!(c, set1(INIT[2])));
    _mm256_storeu_si256(lanes[3].as_mut_ptr().cast(), add!(d, set1(INIT[3])));
    for (lane, key) in out.iter_mut().enumerate() {
        for (word, values) in lanes.iter().enumerate() {
            key[word * 4..word * 4 + 4].copy_from_slice(&values[lane].to_le_bytes());
        }
    }
}
