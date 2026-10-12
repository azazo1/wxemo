// wxemo seed 搜索的 wgpu 计算内核.
//
// 每个 invocation 处理一个候选: seed 由 base + work / wxid_count 推出, 账号标识由
// work % wxid_count 推出, 因此 work 的升序就是 (seed, 账号标识) 的字典序.
//
// 步骤:
//   1. 拼出 "<seed 十进制>" + 账号标识 + "EMOTICON", 总长不超过 55 字节, 只需要一个 MD5 块.
//   2. 在着色器里做单块 MD5, 摘要前 16 字节就是候选密钥.
//   3. 用该密钥做 AES-128 解密. CBC 首块的 IV 就是密钥本身, 所以
//      明文块 = InvCipher(密文块) ^ 密钥.
//   4. 明文块开头命中图片魔数就把 work 写进结果缓冲, 并原子递增计数, 主机据此提前退出.
//
// 表 (S 盒 / 逆 S 盒) 放在只读存储缓冲里, 避免依赖 WGSL 常量数组的动态下标支持.
// MD5 的 K 表与循环移位量直接展开成字面量, AES 的密钥扩展与轮函数同样按常量下标展开.
//
// 本文件由 gen_wgsl.py 生成, 生成脚本与内核一起放在 gpu/ 目录下.

const MAX_HITS: u32 = 64u;

// 每个账号标识的 tail (账号标识 + "EMOTICON") 在 tails 缓冲里占用的 u32 数.
// 主机侧把 "账号标识 + EMOTICON" 整段拼好放进来, 着色器只管逐字节搬运.
const TAIL_STRIDE: u32 = 64u;

struct Params {
    base: u32,
    work_count: u32,
    wxid_count: u32,
    sample_count: u32,
    tail_lengths: array<u32, 8>,
};

@group(0) @binding(0) var<storage, read> params: Params;
@group(0) @binding(1) var<storage, read> tables: array<u32, 512>;
@group(0) @binding(2) var<storage, read> tails: array<u32, 256>;
@group(0) @binding(3) var<storage, read> prefixes: array<u32, 64>;
@group(0) @binding(4) var<storage, read_write> result_count: atomic<u32>;
@group(0) @binding(5) var<storage, read_write> result_indices: array<u32, MAX_HITS>;


// 前 256 项是 AES 正向 S 盒 (密钥扩展用), 后 256 项是逆 S 盒 (解密用).
// 由 aes_sbox() 生成, 已核对 S[0] = 0x63, S[1] = 0x7c, S[0x53] = 0xed.
//   0x63u, 0x7cu, 0x77u, 0x7bu, 0xf2u, 0x6bu, 0x6fu, 0xc5u
//   0x30u, 0x01u, 0x67u, 0x2bu, 0xfeu, 0xd7u, 0xabu, 0x76u
//   0xcau, 0x82u, 0xc9u, 0x7du, 0xfau, 0x59u, 0x47u, 0xf0u
//   0xadu, 0xd4u, 0xa2u, 0xafu, 0x9cu, 0xa4u, 0x72u, 0xc0u
//   0xb7u, 0xfdu, 0x93u, 0x26u, 0x36u, 0x3fu, 0xf7u, 0xccu
//   0x34u, 0xa5u, 0xe5u, 0xf1u, 0x71u, 0xd8u, 0x31u, 0x15u
//   0x04u, 0xc7u, 0x23u, 0xc3u, 0x18u, 0x96u, 0x05u, 0x9au
//   0x07u, 0x12u, 0x80u, 0xe2u, 0xebu, 0x27u, 0xb2u, 0x75u
//   0x09u, 0x83u, 0x2cu, 0x1au, 0x1bu, 0x6eu, 0x5au, 0xa0u
//   0x52u, 0x3bu, 0xd6u, 0xb3u, 0x29u, 0xe3u, 0x2fu, 0x84u
//   0x53u, 0xd1u, 0x00u, 0xedu, 0x20u, 0xfcu, 0xb1u, 0x5bu
//   0x6au, 0xcbu, 0xbeu, 0x39u, 0x4au, 0x4cu, 0x58u, 0xcfu
//   0xd0u, 0xefu, 0xaau, 0xfbu, 0x43u, 0x4du, 0x33u, 0x85u
//   0x45u, 0xf9u, 0x02u, 0x7fu, 0x50u, 0x3cu, 0x9fu, 0xa8u
//   0x51u, 0xa3u, 0x40u, 0x8fu, 0x92u, 0x9du, 0x38u, 0xf5u
//   0xbcu, 0xb6u, 0xdau, 0x21u, 0x10u, 0xffu, 0xf3u, 0xd2u
//   0xcdu, 0x0cu, 0x13u, 0xecu, 0x5fu, 0x97u, 0x44u, 0x17u
//   0xc4u, 0xa7u, 0x7eu, 0x3du, 0x64u, 0x5du, 0x19u, 0x73u
//   0x60u, 0x81u, 0x4fu, 0xdcu, 0x22u, 0x2au, 0x90u, 0x88u
//   0x46u, 0xeeu, 0xb8u, 0x14u, 0xdeu, 0x5eu, 0x0bu, 0xdbu
//   0xe0u, 0x32u, 0x3au, 0x0au, 0x49u, 0x06u, 0x24u, 0x5cu
//   0xc2u, 0xd3u, 0xacu, 0x62u, 0x91u, 0x95u, 0xe4u, 0x79u
//   0xe7u, 0xc8u, 0x37u, 0x6du, 0x8du, 0xd5u, 0x4eu, 0xa9u
//   0x6cu, 0x56u, 0xf4u, 0xeau, 0x65u, 0x7au, 0xaeu, 0x08u
//   0xbau, 0x78u, 0x25u, 0x2eu, 0x1cu, 0xa6u, 0xb4u, 0xc6u
//   0xe8u, 0xddu, 0x74u, 0x1fu, 0x4bu, 0xbdu, 0x8bu, 0x8au
//   0x70u, 0x3eu, 0xb5u, 0x66u, 0x48u, 0x03u, 0xf6u, 0x0eu
//   0x61u, 0x35u, 0x57u, 0xb9u, 0x86u, 0xc1u, 0x1du, 0x9eu
//   0xe1u, 0xf8u, 0x98u, 0x11u, 0x69u, 0xd9u, 0x8eu, 0x94u
//   0x9bu, 0x1eu, 0x87u, 0xe9u, 0xceu, 0x55u, 0x28u, 0xdfu
//   0x8cu, 0xa1u, 0x89u, 0x0du, 0xbfu, 0xe6u, 0x42u, 0x68u
//   0x41u, 0x99u, 0x2du, 0x0fu, 0xb0u, 0x54u, 0xbbu, 0x16u
//   0x52u, 0x09u, 0x6au, 0xd5u, 0x30u, 0x36u, 0xa5u, 0x38u
//   0xbfu, 0x40u, 0xa3u, 0x9eu, 0x81u, 0xf3u, 0xd7u, 0xfbu
//   0x7cu, 0xe3u, 0x39u, 0x82u, 0x9bu, 0x2fu, 0xffu, 0x87u
//   0x34u, 0x8eu, 0x43u, 0x44u, 0xc4u, 0xdeu, 0xe9u, 0xcbu
//   0x54u, 0x7bu, 0x94u, 0x32u, 0xa6u, 0xc2u, 0x23u, 0x3du
//   0xeeu, 0x4cu, 0x95u, 0x0bu, 0x42u, 0xfau, 0xc3u, 0x4eu
//   0x08u, 0x2eu, 0xa1u, 0x66u, 0x28u, 0xd9u, 0x24u, 0xb2u
//   0x76u, 0x5bu, 0xa2u, 0x49u, 0x6du, 0x8bu, 0xd1u, 0x25u
//   0x72u, 0xf8u, 0xf6u, 0x64u, 0x86u, 0x68u, 0x98u, 0x16u
//   0xd4u, 0xa4u, 0x5cu, 0xccu, 0x5du, 0x65u, 0xb6u, 0x92u
//   0x6cu, 0x70u, 0x48u, 0x50u, 0xfdu, 0xedu, 0xb9u, 0xdau
//   0x5eu, 0x15u, 0x46u, 0x57u, 0xa7u, 0x8du, 0x9du, 0x84u
//   0x90u, 0xd8u, 0xabu, 0x00u, 0x8cu, 0xbcu, 0xd3u, 0x0au
//   0xf7u, 0xe4u, 0x58u, 0x05u, 0xb8u, 0xb3u, 0x45u, 0x06u
//   0xd0u, 0x2cu, 0x1eu, 0x8fu, 0xcau, 0x3fu, 0x0fu, 0x02u
//   0xc1u, 0xafu, 0xbdu, 0x03u, 0x01u, 0x13u, 0x8au, 0x6bu
//   0x3au, 0x91u, 0x11u, 0x41u, 0x4fu, 0x67u, 0xdcu, 0xeau
//   0x97u, 0xf2u, 0xcfu, 0xceu, 0xf0u, 0xb4u, 0xe6u, 0x73u
//   0x96u, 0xacu, 0x74u, 0x22u, 0xe7u, 0xadu, 0x35u, 0x85u
//   0xe2u, 0xf9u, 0x37u, 0xe8u, 0x1cu, 0x75u, 0xdfu, 0x6eu
//   0x47u, 0xf1u, 0x1au, 0x71u, 0x1du, 0x29u, 0xc5u, 0x89u
//   0x6fu, 0xb7u, 0x62u, 0x0eu, 0xaau, 0x18u, 0xbeu, 0x1bu
//   0xfcu, 0x56u, 0x3eu, 0x4bu, 0xc6u, 0xd2u, 0x79u, 0x20u
//   0x9au, 0xdbu, 0xc0u, 0xfeu, 0x78u, 0xcdu, 0x5au, 0xf4u
//   0x1fu, 0xddu, 0xa8u, 0x33u, 0x88u, 0x07u, 0xc7u, 0x31u
//   0xb1u, 0x12u, 0x10u, 0x59u, 0x27u, 0x80u, 0xecu, 0x5fu
//   0x60u, 0x51u, 0x7fu, 0xa9u, 0x19u, 0xb5u, 0x4au, 0x0du
//   0x2du, 0xe5u, 0x7au, 0x9fu, 0x93u, 0xc9u, 0x9cu, 0xefu
//   0xa0u, 0xe0u, 0x3bu, 0x4du, 0xaeu, 0x2au, 0xf5u, 0xb0u
//   0xc8u, 0xebu, 0xbbu, 0x3cu, 0x83u, 0x53u, 0x99u, 0x61u
//   0x17u, 0x2bu, 0x04u, 0x7eu, 0xbau, 0x77u, 0xd6u, 0x26u
//   0xe1u, 0x69u, 0x14u, 0x63u, 0x55u, 0x21u, 0x0cu, 0x7du

// ---- MD5 ----

// 单块 MD5 摘要: 输入 16 个消息字, 返回 4 个摘要字.
// 消息一定短于 56 字节, 所以压缩一轮即可, 返回前把初始向量加回去.
fn md5_block(m: array<u32, 16>) -> array<u32, 4> {
    var a = 0x67452301u;
    var b = 0xefcdab89u;
    var c = 0x98badcfeu;
    var d = 0x10325476u;
    var f = 0u;
    var t = 0u;

    // 第 1 组 16 轮, F = (b & c) | (~b & d)
    f = (b & c) | (~b & d);
    t = a + f + m[0] + 0xd76aa478u;
    a = d; d = c; c = b; b = b + rotl(t, 7u);
    f = (b & c) | (~b & d);
    t = a + f + m[1] + 0xe8c7b756u;
    a = d; d = c; c = b; b = b + rotl(t, 12u);
    f = (b & c) | (~b & d);
    t = a + f + m[2] + 0x242070dbu;
    a = d; d = c; c = b; b = b + rotl(t, 17u);
    f = (b & c) | (~b & d);
    t = a + f + m[3] + 0xc1bdceeeu;
    a = d; d = c; c = b; b = b + rotl(t, 22u);
    f = (b & c) | (~b & d);
    t = a + f + m[4] + 0xf57c0fafu;
    a = d; d = c; c = b; b = b + rotl(t, 7u);
    f = (b & c) | (~b & d);
    t = a + f + m[5] + 0x4787c62au;
    a = d; d = c; c = b; b = b + rotl(t, 12u);
    f = (b & c) | (~b & d);
    t = a + f + m[6] + 0xa8304613u;
    a = d; d = c; c = b; b = b + rotl(t, 17u);
    f = (b & c) | (~b & d);
    t = a + f + m[7] + 0xfd469501u;
    a = d; d = c; c = b; b = b + rotl(t, 22u);
    f = (b & c) | (~b & d);
    t = a + f + m[8] + 0x698098d8u;
    a = d; d = c; c = b; b = b + rotl(t, 7u);
    f = (b & c) | (~b & d);
    t = a + f + m[9] + 0x8b44f7afu;
    a = d; d = c; c = b; b = b + rotl(t, 12u);
    f = (b & c) | (~b & d);
    t = a + f + m[10] + 0xffff5bb1u;
    a = d; d = c; c = b; b = b + rotl(t, 17u);
    f = (b & c) | (~b & d);
    t = a + f + m[11] + 0x895cd7beu;
    a = d; d = c; c = b; b = b + rotl(t, 22u);
    f = (b & c) | (~b & d);
    t = a + f + m[12] + 0x6b901122u;
    a = d; d = c; c = b; b = b + rotl(t, 7u);
    f = (b & c) | (~b & d);
    t = a + f + m[13] + 0xfd987193u;
    a = d; d = c; c = b; b = b + rotl(t, 12u);
    f = (b & c) | (~b & d);
    t = a + f + m[14] + 0xa679438eu;
    a = d; d = c; c = b; b = b + rotl(t, 17u);
    f = (b & c) | (~b & d);
    t = a + f + m[15] + 0x49b40821u;
    a = d; d = c; c = b; b = b + rotl(t, 22u);

    // 第 2 组 16 轮, F = (d & b) | (~d & c)
    f = (d & b) | (~d & c);
    t = a + f + m[1] + 0xf61e2562u;
    a = d; d = c; c = b; b = b + rotl(t, 5u);
    f = (d & b) | (~d & c);
    t = a + f + m[6] + 0xc040b340u;
    a = d; d = c; c = b; b = b + rotl(t, 9u);
    f = (d & b) | (~d & c);
    t = a + f + m[11] + 0x265e5a51u;
    a = d; d = c; c = b; b = b + rotl(t, 14u);
    f = (d & b) | (~d & c);
    t = a + f + m[0] + 0xe9b6c7aau;
    a = d; d = c; c = b; b = b + rotl(t, 20u);
    f = (d & b) | (~d & c);
    t = a + f + m[5] + 0xd62f105du;
    a = d; d = c; c = b; b = b + rotl(t, 5u);
    f = (d & b) | (~d & c);
    t = a + f + m[10] + 0x02441453u;
    a = d; d = c; c = b; b = b + rotl(t, 9u);
    f = (d & b) | (~d & c);
    t = a + f + m[15] + 0xd8a1e681u;
    a = d; d = c; c = b; b = b + rotl(t, 14u);
    f = (d & b) | (~d & c);
    t = a + f + m[4] + 0xe7d3fbc8u;
    a = d; d = c; c = b; b = b + rotl(t, 20u);
    f = (d & b) | (~d & c);
    t = a + f + m[9] + 0x21e1cde6u;
    a = d; d = c; c = b; b = b + rotl(t, 5u);
    f = (d & b) | (~d & c);
    t = a + f + m[14] + 0xc33707d6u;
    a = d; d = c; c = b; b = b + rotl(t, 9u);
    f = (d & b) | (~d & c);
    t = a + f + m[3] + 0xf4d50d87u;
    a = d; d = c; c = b; b = b + rotl(t, 14u);
    f = (d & b) | (~d & c);
    t = a + f + m[8] + 0x455a14edu;
    a = d; d = c; c = b; b = b + rotl(t, 20u);
    f = (d & b) | (~d & c);
    t = a + f + m[13] + 0xa9e3e905u;
    a = d; d = c; c = b; b = b + rotl(t, 5u);
    f = (d & b) | (~d & c);
    t = a + f + m[2] + 0xfcefa3f8u;
    a = d; d = c; c = b; b = b + rotl(t, 9u);
    f = (d & b) | (~d & c);
    t = a + f + m[7] + 0x676f02d9u;
    a = d; d = c; c = b; b = b + rotl(t, 14u);
    f = (d & b) | (~d & c);
    t = a + f + m[12] + 0x8d2a4c8au;
    a = d; d = c; c = b; b = b + rotl(t, 20u);

    // 第 3 组 16 轮, F = b ^ c ^ d
    f = b ^ c ^ d;
    t = a + f + m[5] + 0xfffa3942u;
    a = d; d = c; c = b; b = b + rotl(t, 4u);
    f = b ^ c ^ d;
    t = a + f + m[8] + 0x8771f681u;
    a = d; d = c; c = b; b = b + rotl(t, 11u);
    f = b ^ c ^ d;
    t = a + f + m[11] + 0x6d9d6122u;
    a = d; d = c; c = b; b = b + rotl(t, 16u);
    f = b ^ c ^ d;
    t = a + f + m[14] + 0xfde5380cu;
    a = d; d = c; c = b; b = b + rotl(t, 23u);
    f = b ^ c ^ d;
    t = a + f + m[1] + 0xa4beea44u;
    a = d; d = c; c = b; b = b + rotl(t, 4u);
    f = b ^ c ^ d;
    t = a + f + m[4] + 0x4bdecfa9u;
    a = d; d = c; c = b; b = b + rotl(t, 11u);
    f = b ^ c ^ d;
    t = a + f + m[7] + 0xf6bb4b60u;
    a = d; d = c; c = b; b = b + rotl(t, 16u);
    f = b ^ c ^ d;
    t = a + f + m[10] + 0xbebfbc70u;
    a = d; d = c; c = b; b = b + rotl(t, 23u);
    f = b ^ c ^ d;
    t = a + f + m[13] + 0x289b7ec6u;
    a = d; d = c; c = b; b = b + rotl(t, 4u);
    f = b ^ c ^ d;
    t = a + f + m[0] + 0xeaa127fau;
    a = d; d = c; c = b; b = b + rotl(t, 11u);
    f = b ^ c ^ d;
    t = a + f + m[3] + 0xd4ef3085u;
    a = d; d = c; c = b; b = b + rotl(t, 16u);
    f = b ^ c ^ d;
    t = a + f + m[6] + 0x04881d05u;
    a = d; d = c; c = b; b = b + rotl(t, 23u);
    f = b ^ c ^ d;
    t = a + f + m[9] + 0xd9d4d039u;
    a = d; d = c; c = b; b = b + rotl(t, 4u);
    f = b ^ c ^ d;
    t = a + f + m[12] + 0xe6db99e5u;
    a = d; d = c; c = b; b = b + rotl(t, 11u);
    f = b ^ c ^ d;
    t = a + f + m[15] + 0x1fa27cf8u;
    a = d; d = c; c = b; b = b + rotl(t, 16u);
    f = b ^ c ^ d;
    t = a + f + m[2] + 0xc4ac5665u;
    a = d; d = c; c = b; b = b + rotl(t, 23u);

    // 第 4 组 16 轮, F = c ^ (b | ~d)
    f = c ^ (b | ~d);
    t = a + f + m[0] + 0xf4292244u;
    a = d; d = c; c = b; b = b + rotl(t, 6u);
    f = c ^ (b | ~d);
    t = a + f + m[7] + 0x432aff97u;
    a = d; d = c; c = b; b = b + rotl(t, 10u);
    f = c ^ (b | ~d);
    t = a + f + m[14] + 0xab9423a7u;
    a = d; d = c; c = b; b = b + rotl(t, 15u);
    f = c ^ (b | ~d);
    t = a + f + m[5] + 0xfc93a039u;
    a = d; d = c; c = b; b = b + rotl(t, 21u);
    f = c ^ (b | ~d);
    t = a + f + m[12] + 0x655b59c3u;
    a = d; d = c; c = b; b = b + rotl(t, 6u);
    f = c ^ (b | ~d);
    t = a + f + m[3] + 0x8f0ccc92u;
    a = d; d = c; c = b; b = b + rotl(t, 10u);
    f = c ^ (b | ~d);
    t = a + f + m[10] + 0xffeff47du;
    a = d; d = c; c = b; b = b + rotl(t, 15u);
    f = c ^ (b | ~d);
    t = a + f + m[1] + 0x85845dd1u;
    a = d; d = c; c = b; b = b + rotl(t, 21u);
    f = c ^ (b | ~d);
    t = a + f + m[8] + 0x6fa87e4fu;
    a = d; d = c; c = b; b = b + rotl(t, 6u);
    f = c ^ (b | ~d);
    t = a + f + m[15] + 0xfe2ce6e0u;
    a = d; d = c; c = b; b = b + rotl(t, 10u);
    f = c ^ (b | ~d);
    t = a + f + m[6] + 0xa3014314u;
    a = d; d = c; c = b; b = b + rotl(t, 15u);
    f = c ^ (b | ~d);
    t = a + f + m[13] + 0x4e0811a1u;
    a = d; d = c; c = b; b = b + rotl(t, 21u);
    f = c ^ (b | ~d);
    t = a + f + m[4] + 0xf7537e82u;
    a = d; d = c; c = b; b = b + rotl(t, 6u);
    f = c ^ (b | ~d);
    t = a + f + m[11] + 0xbd3af235u;
    a = d; d = c; c = b; b = b + rotl(t, 10u);
    f = c ^ (b | ~d);
    t = a + f + m[2] + 0x2ad7d2bbu;
    a = d; d = c; c = b; b = b + rotl(t, 15u);
    f = c ^ (b | ~d);
    t = a + f + m[9] + 0xeb86d391u;
    a = d; d = c; c = b; b = b + rotl(t, 21u);

    return array<u32, 4>(
        a + 0x67452301u,
        b + 0xefcdab89u,
        c + 0x98badcfeu,
        d + 0x10325476u,
    );
}

// ---- AES-128 ----

// InvShiftRows: 第 r 行循环右移 r 个字节, 等价于新列 c 的第 r 行取自老列 (c - r) mod 4.
fn inv_shift_rows(s: array<u32, 4>) -> array<u32, 4> {
    let t0 = s[0];
    let t1 = s[1];
    let t2 = s[2];
    let t3 = s[3];
    // 每个字的高字节是第 0 行, 最低字节是第 3 行.
    let o0 = (t0 & 0xff000000u) | (t3 & 0x00ff0000u) | (t2 & 0x0000ff00u) | (t1 & 0x000000ffu);
    let o1 = (t1 & 0xff000000u) | (t0 & 0x00ff0000u) | (t3 & 0x0000ff00u) | (t2 & 0x000000ffu);
    let o2 = (t2 & 0xff000000u) | (t1 & 0x00ff0000u) | (t0 & 0x0000ff00u) | (t3 & 0x000000ffu);
    let o3 = (t3 & 0xff000000u) | (t2 & 0x00ff0000u) | (t1 & 0x0000ff00u) | (t0 & 0x000000ffu);
    return array<u32, 4>(o0, o1, o2, o3);
}

// InvMixColumns: 每列在 GF(2^8) 上乘逆矩阵, 系数 09 / 0b / 0d / 0e 用 xtime 组合出来.
fn inv_mix_column(w: u32) -> u32 {
    let a0 = (w >> 24u) & 0xffu;
    let a1 = (w >> 16u) & 0xffu;
    let a2 = (w >> 8u) & 0xffu;
    let a3 = w & 0xffu;
    let b02 = xtime(a0);
    let b04 = xtime(b02);
    let b08 = xtime(b04);
    let m09 = b08 ^ a0;
    let m0b = b08 ^ b02 ^ a0;
    let m0d = b08 ^ b04 ^ a0;
    let m0e = b08 ^ b04 ^ b02;
    let b12 = xtime(a1);
    let b14 = xtime(b12);
    let b18 = xtime(b14);
    let m19 = b18 ^ a1;
    let m1b = b18 ^ b12 ^ a1;
    let m1d = b18 ^ b14 ^ a1;
    let m1e = b18 ^ b14 ^ b12;
    let b22 = xtime(a2);
    let b24 = xtime(b22);
    let b28 = xtime(b24);
    let m29 = b28 ^ a2;
    let m2b = b28 ^ b22 ^ a2;
    let m2d = b28 ^ b24 ^ a2;
    let m2e = b28 ^ b24 ^ b22;
    let b32 = xtime(a3);
    let b34 = xtime(b32);
    let b38 = xtime(b34);
    let m39 = b38 ^ a3;
    let m3b = b38 ^ b32 ^ a3;
    let m3d = b38 ^ b34 ^ a3;
    let m3e = b38 ^ b34 ^ b32;
    let r0 = m0e ^ m1b ^ m2d ^ m39;
    let r1 = m09 ^ m1e ^ m2b ^ m3d;
    let r2 = m0d ^ m19 ^ m2e ^ m3b;
    let r3 = m0b ^ m1d ^ m29 ^ m3e;
    return (r0 << 24u) | (r1 << 16u) | (r2 << 8u) | r3;
}

// AES-128 密钥扩展, 产生 44 个轮密钥字 (每个字是一列的 4 个字节, 大端序).
fn aes_expand(k0: u32, k1: u32, k2: u32, k3: u32) -> array<u32, 44> {
    var rk: array<u32, 44>;
    rk[0] = k0;
    rk[1] = k1;
    rk[2] = k2;
    rk[3] = k3;
    rk[4] = rk[0] ^ sub_word(rot_word(rk[3])) ^ 0x01000000u;
    rk[5] = rk[1] ^ rk[4];
    rk[6] = rk[2] ^ rk[5];
    rk[7] = rk[3] ^ rk[6];
    rk[8] = rk[4] ^ sub_word(rot_word(rk[7])) ^ 0x02000000u;
    rk[9] = rk[5] ^ rk[8];
    rk[10] = rk[6] ^ rk[9];
    rk[11] = rk[7] ^ rk[10];
    rk[12] = rk[8] ^ sub_word(rot_word(rk[11])) ^ 0x04000000u;
    rk[13] = rk[9] ^ rk[12];
    rk[14] = rk[10] ^ rk[13];
    rk[15] = rk[11] ^ rk[14];
    rk[16] = rk[12] ^ sub_word(rot_word(rk[15])) ^ 0x08000000u;
    rk[17] = rk[13] ^ rk[16];
    rk[18] = rk[14] ^ rk[17];
    rk[19] = rk[15] ^ rk[18];
    rk[20] = rk[16] ^ sub_word(rot_word(rk[19])) ^ 0x10000000u;
    rk[21] = rk[17] ^ rk[20];
    rk[22] = rk[18] ^ rk[21];
    rk[23] = rk[19] ^ rk[22];
    rk[24] = rk[20] ^ sub_word(rot_word(rk[23])) ^ 0x20000000u;
    rk[25] = rk[21] ^ rk[24];
    rk[26] = rk[22] ^ rk[25];
    rk[27] = rk[23] ^ rk[26];
    rk[28] = rk[24] ^ sub_word(rot_word(rk[27])) ^ 0x40000000u;
    rk[29] = rk[25] ^ rk[28];
    rk[30] = rk[26] ^ rk[29];
    rk[31] = rk[27] ^ rk[30];
    rk[32] = rk[28] ^ sub_word(rot_word(rk[31])) ^ 0x80000000u;
    rk[33] = rk[29] ^ rk[32];
    rk[34] = rk[30] ^ rk[33];
    rk[35] = rk[31] ^ rk[34];
    rk[36] = rk[32] ^ sub_word(rot_word(rk[35])) ^ 0x1b000000u;
    rk[37] = rk[33] ^ rk[36];
    rk[38] = rk[34] ^ rk[37];
    rk[39] = rk[35] ^ rk[38];
    rk[40] = rk[36] ^ sub_word(rot_word(rk[39])) ^ 0x36000000u;
    rk[41] = rk[37] ^ rk[40];
    rk[42] = rk[38] ^ rk[41];
    rk[43] = rk[39] ^ rk[42];
    return rk;
}

// 解密单个 16 字节块 (InvCipher), 输入密文块与密钥字, 返回 InvCipher 的结果.
// 调用方再与密钥异或得到 CBC 首块明文.
fn aes_inv_cipher(ct: array<u32, 4>, k0: u32, k1: u32, k2: u32, k3: u32) -> array<u32, 4> {
    let rk = aes_expand(k0, k1, k2, k3);
    var s0 = ct[0] ^ rk[40];
    var s1 = ct[1] ^ rk[41];
    var s2 = ct[2] ^ rk[42];
    var s3 = ct[3] ^ rk[43];

    // 第 9 轮
    {
        let t = inv_shift_rows(array<u32, 4>(s0, s1, s2, s3));
        s0 = inv_sub_word(t[0]);
        s1 = inv_sub_word(t[1]);
        s2 = inv_sub_word(t[2]);
        s3 = inv_sub_word(t[3]);
        s0 = s0 ^ rk[36];
        s1 = s1 ^ rk[37];
        s2 = s2 ^ rk[38];
        s3 = s3 ^ rk[39];
        s0 = inv_mix_column(s0);
        s1 = inv_mix_column(s1);
        s2 = inv_mix_column(s2);
        s3 = inv_mix_column(s3);
    }

    // 第 8 轮
    {
        let t = inv_shift_rows(array<u32, 4>(s0, s1, s2, s3));
        s0 = inv_sub_word(t[0]);
        s1 = inv_sub_word(t[1]);
        s2 = inv_sub_word(t[2]);
        s3 = inv_sub_word(t[3]);
        s0 = s0 ^ rk[32];
        s1 = s1 ^ rk[33];
        s2 = s2 ^ rk[34];
        s3 = s3 ^ rk[35];
        s0 = inv_mix_column(s0);
        s1 = inv_mix_column(s1);
        s2 = inv_mix_column(s2);
        s3 = inv_mix_column(s3);
    }

    // 第 7 轮
    {
        let t = inv_shift_rows(array<u32, 4>(s0, s1, s2, s3));
        s0 = inv_sub_word(t[0]);
        s1 = inv_sub_word(t[1]);
        s2 = inv_sub_word(t[2]);
        s3 = inv_sub_word(t[3]);
        s0 = s0 ^ rk[28];
        s1 = s1 ^ rk[29];
        s2 = s2 ^ rk[30];
        s3 = s3 ^ rk[31];
        s0 = inv_mix_column(s0);
        s1 = inv_mix_column(s1);
        s2 = inv_mix_column(s2);
        s3 = inv_mix_column(s3);
    }

    // 第 6 轮
    {
        let t = inv_shift_rows(array<u32, 4>(s0, s1, s2, s3));
        s0 = inv_sub_word(t[0]);
        s1 = inv_sub_word(t[1]);
        s2 = inv_sub_word(t[2]);
        s3 = inv_sub_word(t[3]);
        s0 = s0 ^ rk[24];
        s1 = s1 ^ rk[25];
        s2 = s2 ^ rk[26];
        s3 = s3 ^ rk[27];
        s0 = inv_mix_column(s0);
        s1 = inv_mix_column(s1);
        s2 = inv_mix_column(s2);
        s3 = inv_mix_column(s3);
    }

    // 第 5 轮
    {
        let t = inv_shift_rows(array<u32, 4>(s0, s1, s2, s3));
        s0 = inv_sub_word(t[0]);
        s1 = inv_sub_word(t[1]);
        s2 = inv_sub_word(t[2]);
        s3 = inv_sub_word(t[3]);
        s0 = s0 ^ rk[20];
        s1 = s1 ^ rk[21];
        s2 = s2 ^ rk[22];
        s3 = s3 ^ rk[23];
        s0 = inv_mix_column(s0);
        s1 = inv_mix_column(s1);
        s2 = inv_mix_column(s2);
        s3 = inv_mix_column(s3);
    }

    // 第 4 轮
    {
        let t = inv_shift_rows(array<u32, 4>(s0, s1, s2, s3));
        s0 = inv_sub_word(t[0]);
        s1 = inv_sub_word(t[1]);
        s2 = inv_sub_word(t[2]);
        s3 = inv_sub_word(t[3]);
        s0 = s0 ^ rk[16];
        s1 = s1 ^ rk[17];
        s2 = s2 ^ rk[18];
        s3 = s3 ^ rk[19];
        s0 = inv_mix_column(s0);
        s1 = inv_mix_column(s1);
        s2 = inv_mix_column(s2);
        s3 = inv_mix_column(s3);
    }

    // 第 3 轮
    {
        let t = inv_shift_rows(array<u32, 4>(s0, s1, s2, s3));
        s0 = inv_sub_word(t[0]);
        s1 = inv_sub_word(t[1]);
        s2 = inv_sub_word(t[2]);
        s3 = inv_sub_word(t[3]);
        s0 = s0 ^ rk[12];
        s1 = s1 ^ rk[13];
        s2 = s2 ^ rk[14];
        s3 = s3 ^ rk[15];
        s0 = inv_mix_column(s0);
        s1 = inv_mix_column(s1);
        s2 = inv_mix_column(s2);
        s3 = inv_mix_column(s3);
    }

    // 第 2 轮
    {
        let t = inv_shift_rows(array<u32, 4>(s0, s1, s2, s3));
        s0 = inv_sub_word(t[0]);
        s1 = inv_sub_word(t[1]);
        s2 = inv_sub_word(t[2]);
        s3 = inv_sub_word(t[3]);
        s0 = s0 ^ rk[8];
        s1 = s1 ^ rk[9];
        s2 = s2 ^ rk[10];
        s3 = s3 ^ rk[11];
        s0 = inv_mix_column(s0);
        s1 = inv_mix_column(s1);
        s2 = inv_mix_column(s2);
        s3 = inv_mix_column(s3);
    }

    // 第 1 轮
    {
        let t = inv_shift_rows(array<u32, 4>(s0, s1, s2, s3));
        s0 = inv_sub_word(t[0]);
        s1 = inv_sub_word(t[1]);
        s2 = inv_sub_word(t[2]);
        s3 = inv_sub_word(t[3]);
        s0 = s0 ^ rk[4];
        s1 = s1 ^ rk[5];
        s2 = s2 ^ rk[6];
        s3 = s3 ^ rk[7];
        s0 = inv_mix_column(s0);
        s1 = inv_mix_column(s1);
        s2 = inv_mix_column(s2);
        s3 = inv_mix_column(s3);
    }

    // 最后一轮不再做 InvMixColumns
    {
        let t = inv_shift_rows(array<u32, 4>(s0, s1, s2, s3));
        s0 = inv_sub_word(t[0]) ^ rk[0];
        s1 = inv_sub_word(t[1]) ^ rk[1];
        s2 = inv_sub_word(t[2]) ^ rk[2];
        s3 = inv_sub_word(t[3]) ^ rk[3];
    }
    return array<u32, 4>(s0, s1, s2, s3);
}

// 十进制位数, 0 算一位, 与 CPU 侧 write_decimal 的 do-while 一致.
fn digit_count(v: u32) -> u32 {
    if (v >= 1000000000u) { return 10u; }
    if (v >= 100000000u) { return 9u; }
    if (v >= 10000000u) { return 8u; }
    if (v >= 1000000u) { return 7u; }
    if (v >= 100000u) { return 6u; }
    if (v >= 10000u) { return 5u; }
    if (v >= 1000u) { return 4u; }
    if (v >= 100u) { return 3u; }
    if (v >= 10u) { return 2u; }
    return 1u;
}

// 把消息的第 pos 个字节或进消息字数组, 小端序.
fn put_byte(msg: ptr<function, array<u32, 16>>, pos: u32, value: u32) {
    let word = pos / 4u;
    let shift = (pos % 4u) * 8u;
    (*msg)[word] = (*msg)[word] | (value << shift);
}

// 32 位循环左移. WGSL 没有内置的 rotateLeft, 这里自己拼.
// 只在 1..31 的移位量上使用.
fn rotl(x: u32, n: u32) -> u32 {
    return (x << n) | (x >> (32u - n));
}

fn rot_word(w: u32) -> u32 {
    return rotl(w, 8u);
}

fn sub_word(w: u32) -> u32 {
    return (sbox((w >> 24u) & 0xffu) << 24u)
        | (sbox((w >> 16u) & 0xffu) << 16u)
        | (sbox((w >> 8u) & 0xffu) << 8u)
        | sbox(w & 0xffu);
}

fn inv_sub_word(w: u32) -> u32 {
    return (inv_sbox((w >> 24u) & 0xffu) << 24u)
        | (inv_sbox((w >> 16u) & 0xffu) << 16u)
        | (inv_sbox((w >> 8u) & 0xffu) << 8u)
        | inv_sbox(w & 0xffu);
}

fn sbox(x: u32) -> u32 {
    return tables[x];
}

fn inv_sbox(x: u32) -> u32 {
    return tables[256u + x];
}

fn xtime(x: u32) -> u32 {
    let doubled = (x << 1u) & 0xffu;
    return select(doubled, doubled ^ 0x1bu, (x & 0x80u) != 0u);
}

// 字节序反转: MD5 摘要按小端序输出, AES 的一个字按大端序解释.
fn bswap(x: u32) -> u32 {
    return ((x & 0x000000ffu) << 24u)
        | ((x & 0x0000ff00u) << 8u)
        | ((x & 0x00ff0000u) >> 8u)
        | ((x & 0xff000000u) >> 24u);
}

// 判定明文块开头是否命中图片魔数, 与 CPU 侧 match_magic 的判定完全一致.
// 这里的 p0..p3 是明文块的 4 个大端序字, 也就是前 4 / 8 / 12 / 16 个字节.
fn matches_magic(p0: u32, p1: u32, p2: u32) -> bool {
    // png: 89 50 4e 47 0d 0a 1a 0a
    if (p0 == 0x89504e47u && p1 == 0x0d0a1a0au) { return true; }
    // gif87a: 47 49 46 38 37 61
    if (p0 == 0x47494638u && (p1 & 0xffff0000u) == 0x37610000u) { return true; }
    // gif89a: 47 49 46 38 39 61
    if (p0 == 0x47494638u && (p1 & 0xffff0000u) == 0x39610000u) { return true; }
    // jpg: ff d8 ff
    if ((p0 & 0xffffff00u) == 0xffd8ff00u) { return true; }
    // wxgf: 77 78 67 66
    if (p0 == 0x77786766u) { return true; }
    // webp: 52 49 46 46 .. .. .. .. 57 45 42 50
    if (p0 == 0x52494646u && p2 == 0x57454250u) { return true; }
    return false;
}

@compute @workgroup_size(128)
fn main(@builtin(global_invocation_id) gid: vec3<u32>) {
    let work = gid.x;
    if (work >= params.work_count) { return; }
    // 同批里已经有候选命中, 后续 invocation 直接退出.
    if (atomicLoad(&result_count) >= MAX_HITS) { return; }

    let seed = params.base + work / params.wxid_count;
    let wxid_index = work % params.wxid_count;
    let tail_offset = wxid_index * TAIL_STRIDE;
    let tail_len = params.tail_lengths[wxid_index];
    let digits = digit_count(seed);

    // 1. 拼消息: 十进制 seed + 账号标识 + "EMOTICON", 再补 MD5 的 0x80 与长度.
    var msg: array<u32, 16>;
    var rest = seed;
    var pos = digits;
    loop {
        pos = pos - 1u;
        put_byte(&msg, pos, 0x30u + (rest % 10u));
        rest = rest / 10u;
        if (pos == 0u) { break; }
    }
    for (var k = 0u; k < tail_len; k = k + 1u) {
        put_byte(&msg, digits + k, tails[tail_offset + k]);
    }
    let used = digits + tail_len;
    put_byte(&msg, used, 0x80u);
    // 消息短于 56 字节, 位长度写进第 14 个字, 第 15 个字保持 0.
    msg[14] = used * 8u;

    // 2. 单块 MD5, 摘要前 16 字节即密钥. 密钥字按大端序解释.
    let digest = md5_block(msg);
    let k0 = bswap(digest[0]);
    let k1 = bswap(digest[1]);
    let k2 = bswap(digest[2]);
    let k3 = bswap(digest[3]);

    // 3. 逐个样本判定, 第一个不匹配就丢掉.
    for (var s = 0u; s < params.sample_count; s = s + 1u) {
        let ct = array<u32, 4>(
            prefixes[s * 4u],
            prefixes[s * 4u + 1u],
            prefixes[s * 4u + 2u],
            prefixes[s * 4u + 3u],
        );
        let dec = aes_inv_cipher(ct, k0, k1, k2, k3);
        let p0 = dec[0] ^ k0;
        let p1 = dec[1] ^ k1;
        let p2 = dec[2] ^ k2;
        if (!matches_magic(p0, p1, p2)) { return; }
    }

    // 4. 全部样本通过, 记下候选序号.
    let slot = atomicAdd(&result_count, 1u);
    if (slot < MAX_HITS) {
        result_indices[slot] = work;
    }
}

