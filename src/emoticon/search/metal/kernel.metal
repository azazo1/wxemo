// wxemo seed 搜索的 Metal compute kernel.
//
// 每个 GPU 线程负责一个候选 seed:
//   1. 拼出消息 "<seed 十进制>" + wxid + "EMOTICON"
//   2. 单块 MD5 取前 16 字节作为 AES-128 密钥
//   3. 对每个样本的密文首块做 AES-128-CBC 解密, IV 等于密钥, 再与密钥异或
//   4. 明文首块命中图片魔数就写入结果缓冲, 并用原子标志通知主机提前退出
//
// 消息长度必须小于 56 字节才能只用一个 MD5 块, 主机侧在提交前会校验.
// 判定顺序与 CPU 后端一致: 先看第一个样本, 不匹配就直接丢弃.

#include <metal_stdlib>

using namespace metal;

// ---------------------------------------------------------------------------
// 常量表
// ---------------------------------------------------------------------------

// MD5 的 T 表, 即 floor(abs(sin(i + 1)) * 2^32).
constant uint MD5_K[64] = {
    0xd76aa478u, 0xe8c7b756u, 0x242070dbu, 0xc1bdceeeu,
    0xf57c0fafu, 0x4787c62au, 0xa8304613u, 0xfd469501u,
    0x698098d8u, 0x8b44f7afu, 0xffff5bb1u, 0x895cd7beu,
    0x6b901122u, 0xfd987193u, 0xa679438eu, 0x49b40821u,
    0xf61e2562u, 0xc040b340u, 0x265e5a51u, 0xe9b6c7aau,
    0xd62f105du, 0x02441453u, 0xd8a1e681u, 0xe7d3fbc8u,
    0x21e1cde6u, 0xc33707d6u, 0xf4d50d87u, 0x455a14edu,
    0xa9e3e905u, 0xfcefa3f8u, 0x676f02d9u, 0x8d2a4c8au,
    0xfffa3942u, 0x8771f681u, 0x6d9d6122u, 0xfde5380cu,
    0xa4beea44u, 0x4bdecfa9u, 0xf6bb4b60u, 0xbebfbc70u,
    0x289b7ec6u, 0xeaa127fau, 0xd4ef3085u, 0x04881d05u,
    0xd9d4d039u, 0xe6db99e5u, 0x1fa27cf8u, 0xc4ac5665u,
    0xf4292244u, 0x432aff97u, 0xab9423a7u, 0xfc93a039u,
    0x655b59c3u, 0x8f0ccc92u, 0xffeff47du, 0x85845dd1u,
    0x6fa87e4fu, 0xfe2ce6e0u, 0xa3014314u, 0x4e0811a1u,
    0xf7537e82u, 0xbd3af235u, 0x2ad7d2bbu, 0xeb86d391u,
};

// MD5 每轮的循环左移位数.
constant uchar MD5_S[64] = {
    0x07, 0x0c, 0x11, 0x16, 0x07, 0x0c, 0x11, 0x16, 0x07, 0x0c, 0x11, 0x16, 0x07, 0x0c, 0x11, 0x16,
    0x05, 0x09, 0x0e, 0x14, 0x05, 0x09, 0x0e, 0x14, 0x05, 0x09, 0x0e, 0x14, 0x05, 0x09, 0x0e, 0x14,
    0x04, 0x0b, 0x10, 0x17, 0x04, 0x0b, 0x10, 0x17, 0x04, 0x0b, 0x10, 0x17, 0x04, 0x0b, 0x10, 0x17,
    0x06, 0x0a, 0x0f, 0x15, 0x06, 0x0a, 0x0f, 0x15, 0x06, 0x0a, 0x0f, 0x15, 0x06, 0x0a, 0x0f, 0x15,
};

// AES 的 S 盒.
constant uchar AES_SBOX[256] = {
    0x63, 0x7c, 0x77, 0x7b, 0xf2, 0x6b, 0x6f, 0xc5, 0x30, 0x01, 0x67, 0x2b, 0xfe, 0xd7, 0xab, 0x76,
    0xca, 0x82, 0xc9, 0x7d, 0xfa, 0x59, 0x47, 0xf0, 0xad, 0xd4, 0xa2, 0xaf, 0x9c, 0xa4, 0x72, 0xc0,
    0xb7, 0xfd, 0x93, 0x26, 0x36, 0x3f, 0xf7, 0xcc, 0x34, 0xa5, 0xe5, 0xf1, 0x71, 0xd8, 0x31, 0x15,
    0x04, 0xc7, 0x23, 0xc3, 0x18, 0x96, 0x05, 0x9a, 0x07, 0x12, 0x80, 0xe2, 0xeb, 0x27, 0xb2, 0x75,
    0x09, 0x83, 0x2c, 0x1a, 0x1b, 0x6e, 0x5a, 0xa0, 0x52, 0x3b, 0xd6, 0xb3, 0x29, 0xe3, 0x2f, 0x84,
    0x53, 0xd1, 0x00, 0xed, 0x20, 0xfc, 0xb1, 0x5b, 0x6a, 0xcb, 0xbe, 0x39, 0x4a, 0x4c, 0x58, 0xcf,
    0xd0, 0xef, 0xaa, 0xfb, 0x43, 0x4d, 0x33, 0x85, 0x45, 0xf9, 0x02, 0x7f, 0x50, 0x3c, 0x9f, 0xa8,
    0x51, 0xa3, 0x40, 0x8f, 0x92, 0x9d, 0x38, 0xf5, 0xbc, 0xb6, 0xda, 0x21, 0x10, 0xff, 0xf3, 0xd2,
    0xcd, 0x0c, 0x13, 0xec, 0x5f, 0x97, 0x44, 0x17, 0xc4, 0xa7, 0x7e, 0x3d, 0x64, 0x5d, 0x19, 0x73,
    0x60, 0x81, 0x4f, 0xdc, 0x22, 0x2a, 0x90, 0x88, 0x46, 0xee, 0xb8, 0x14, 0xde, 0x5e, 0x0b, 0xdb,
    0xe0, 0x32, 0x3a, 0x0a, 0x49, 0x06, 0x24, 0x5c, 0xc2, 0xd3, 0xac, 0x62, 0x91, 0x95, 0xe4, 0x79,
    0xe7, 0xc8, 0x37, 0x6d, 0x8d, 0xd5, 0x4e, 0xa9, 0x6c, 0x56, 0xf4, 0xea, 0x65, 0x7a, 0xae, 0x08,
    0xba, 0x78, 0x25, 0x2e, 0x1c, 0xa6, 0xb4, 0xc6, 0xe8, 0xdd, 0x74, 0x1f, 0x4b, 0xbd, 0x8b, 0x8a,
    0x70, 0x3e, 0xb5, 0x66, 0x48, 0x03, 0xf6, 0x0e, 0x61, 0x35, 0x57, 0xb9, 0x86, 0xc1, 0x1d, 0x9e,
    0xe1, 0xf8, 0x98, 0x11, 0x69, 0xd9, 0x8e, 0x94, 0x9b, 0x1e, 0x87, 0xe9, 0xce, 0x55, 0x28, 0xdf,
    0x8c, 0xa1, 0x89, 0x0d, 0xbf, 0xe6, 0x42, 0x68, 0x41, 0x99, 0x2d, 0x0f, 0xb0, 0x54, 0xbb, 0x16,
};

// AES 的逆 S 盒.
constant uchar AES_INV_SBOX[256] = {
    0x52, 0x09, 0x6a, 0xd5, 0x30, 0x36, 0xa5, 0x38, 0xbf, 0x40, 0xa3, 0x9e, 0x81, 0xf3, 0xd7, 0xfb,
    0x7c, 0xe3, 0x39, 0x82, 0x9b, 0x2f, 0xff, 0x87, 0x34, 0x8e, 0x43, 0x44, 0xc4, 0xde, 0xe9, 0xcb,
    0x54, 0x7b, 0x94, 0x32, 0xa6, 0xc2, 0x23, 0x3d, 0xee, 0x4c, 0x95, 0x0b, 0x42, 0xfa, 0xc3, 0x4e,
    0x08, 0x2e, 0xa1, 0x66, 0x28, 0xd9, 0x24, 0xb2, 0x76, 0x5b, 0xa2, 0x49, 0x6d, 0x8b, 0xd1, 0x25,
    0x72, 0xf8, 0xf6, 0x64, 0x86, 0x68, 0x98, 0x16, 0xd4, 0xa4, 0x5c, 0xcc, 0x5d, 0x65, 0xb6, 0x92,
    0x6c, 0x70, 0x48, 0x50, 0xfd, 0xed, 0xb9, 0xda, 0x5e, 0x15, 0x46, 0x57, 0xa7, 0x8d, 0x9d, 0x84,
    0x90, 0xd8, 0xab, 0x00, 0x8c, 0xbc, 0xd3, 0x0a, 0xf7, 0xe4, 0x58, 0x05, 0xb8, 0xb3, 0x45, 0x06,
    0xd0, 0x2c, 0x1e, 0x8f, 0xca, 0x3f, 0x0f, 0x02, 0xc1, 0xaf, 0xbd, 0x03, 0x01, 0x13, 0x8a, 0x6b,
    0x3a, 0x91, 0x11, 0x41, 0x4f, 0x67, 0xdc, 0xea, 0x97, 0xf2, 0xcf, 0xce, 0xf0, 0xb4, 0xe6, 0x73,
    0x96, 0xac, 0x74, 0x22, 0xe7, 0xad, 0x35, 0x85, 0xe2, 0xf9, 0x37, 0xe8, 0x1c, 0x75, 0xdf, 0x6e,
    0x47, 0xf1, 0x1a, 0x71, 0x1d, 0x29, 0xc5, 0x89, 0x6f, 0xb7, 0x62, 0x0e, 0xaa, 0x18, 0xbe, 0x1b,
    0xfc, 0x56, 0x3e, 0x4b, 0xc6, 0xd2, 0x79, 0x20, 0x9a, 0xdb, 0xc0, 0xfe, 0x78, 0xcd, 0x5a, 0xf4,
    0x1f, 0xdd, 0xa8, 0x33, 0x88, 0x07, 0xc7, 0x31, 0xb1, 0x12, 0x10, 0x59, 0x27, 0x80, 0xec, 0x5f,
    0x60, 0x51, 0x7f, 0xa9, 0x19, 0xb5, 0x4a, 0x0d, 0x2d, 0xe5, 0x7a, 0x9f, 0x93, 0xc9, 0x9c, 0xef,
    0xa0, 0xe0, 0x3b, 0x4d, 0xae, 0x2a, 0xf5, 0xb0, 0xc8, 0xeb, 0xbb, 0x3c, 0x83, 0x53, 0x99, 0x61,
    0x17, 0x2b, 0x04, 0x7e, 0xba, 0x77, 0xd6, 0x26, 0xe1, 0x69, 0x14, 0x63, 0x55, 0x21, 0x0c, 0x7d,
};

// 字节乘以 9 (GF(2^8)), 用于 InvMixColumns 里的 9 * 列异或和.
constant uchar AES_MUL9[256] = {
    0x00, 0x09, 0x12, 0x1b, 0x24, 0x2d, 0x36, 0x3f, 0x48, 0x41, 0x5a, 0x53, 0x6c, 0x65, 0x7e, 0x77,
    0x90, 0x99, 0x82, 0x8b, 0xb4, 0xbd, 0xa6, 0xaf, 0xd8, 0xd1, 0xca, 0xc3, 0xfc, 0xf5, 0xee, 0xe7,
    0x3b, 0x32, 0x29, 0x20, 0x1f, 0x16, 0x0d, 0x04, 0x73, 0x7a, 0x61, 0x68, 0x57, 0x5e, 0x45, 0x4c,
    0xab, 0xa2, 0xb9, 0xb0, 0x8f, 0x86, 0x9d, 0x94, 0xe3, 0xea, 0xf1, 0xf8, 0xc7, 0xce, 0xd5, 0xdc,
    0x76, 0x7f, 0x64, 0x6d, 0x52, 0x5b, 0x40, 0x49, 0x3e, 0x37, 0x2c, 0x25, 0x1a, 0x13, 0x08, 0x01,
    0xe6, 0xef, 0xf4, 0xfd, 0xc2, 0xcb, 0xd0, 0xd9, 0xae, 0xa7, 0xbc, 0xb5, 0x8a, 0x83, 0x98, 0x91,
    0x4d, 0x44, 0x5f, 0x56, 0x69, 0x60, 0x7b, 0x72, 0x05, 0x0c, 0x17, 0x1e, 0x21, 0x28, 0x33, 0x3a,
    0xdd, 0xd4, 0xcf, 0xc6, 0xf9, 0xf0, 0xeb, 0xe2, 0x95, 0x9c, 0x87, 0x8e, 0xb1, 0xb8, 0xa3, 0xaa,
    0xec, 0xe5, 0xfe, 0xf7, 0xc8, 0xc1, 0xda, 0xd3, 0xa4, 0xad, 0xb6, 0xbf, 0x80, 0x89, 0x92, 0x9b,
    0x7c, 0x75, 0x6e, 0x67, 0x58, 0x51, 0x4a, 0x43, 0x34, 0x3d, 0x26, 0x2f, 0x10, 0x19, 0x02, 0x0b,
    0xd7, 0xde, 0xc5, 0xcc, 0xf3, 0xfa, 0xe1, 0xe8, 0x9f, 0x96, 0x8d, 0x84, 0xbb, 0xb2, 0xa9, 0xa0,
    0x47, 0x4e, 0x55, 0x5c, 0x63, 0x6a, 0x71, 0x78, 0x0f, 0x06, 0x1d, 0x14, 0x2b, 0x22, 0x39, 0x30,
    0x9a, 0x93, 0x88, 0x81, 0xbe, 0xb7, 0xac, 0xa5, 0xd2, 0xdb, 0xc0, 0xc9, 0xf6, 0xff, 0xe4, 0xed,
    0x0a, 0x03, 0x18, 0x11, 0x2e, 0x27, 0x3c, 0x35, 0x42, 0x4b, 0x50, 0x59, 0x66, 0x6f, 0x74, 0x7d,
    0xa1, 0xa8, 0xb3, 0xba, 0x85, 0x8c, 0x97, 0x9e, 0xe9, 0xe0, 0xfb, 0xf2, 0xcd, 0xc4, 0xdf, 0xd6,
    0x31, 0x38, 0x23, 0x2a, 0x15, 0x1c, 0x07, 0x0e, 0x79, 0x70, 0x6b, 0x62, 0x5d, 0x54, 0x4f, 0x46,
};

// AES 密钥扩展的轮常量, 在字内按小端字节序摆放.
constant uchar AES_RCON[10] = {
    0x01, 0x02, 0x04, 0x08, 0x10, 0x20, 0x40, 0x80, 0x1b, 0x36,
};

// 消息末尾的固定后缀.
constant uchar MSG_SUFFIX[8] = {
    'E', 'M', 'O', 'T', 'I', 'C', 'O', 'N',
};

// ---------------------------------------------------------------------------
// 参数与缓冲布局
// ---------------------------------------------------------------------------

// 一个账号标识在 wxid_data 里的位置.
struct WxidEntry {
    uint offset;
    uint length;
};

// 一次提交的参数.
struct SeedParams {
    ulong base;          // 本批第一个候选 seed
    uint count;          // 每个账号标识覆盖的候选数
    uint wxid_count;     // 账号标识数量
    uint sample_count;   // 样本数量
    uint max_hits;       // 结果缓冲容量
};

// ---------------------------------------------------------------------------
// 小工具
// ---------------------------------------------------------------------------

static inline uint load_le32_thread(thread const uchar* p) {
    return (uint)p[0] | ((uint)p[1] << 8) | ((uint)p[2] << 16) | ((uint)p[3] << 24);
}

static inline uint load_le32_device(device const uchar* p) {
    return (uint)p[0] | ((uint)p[1] << 8) | ((uint)p[2] << 16) | ((uint)p[3] << 24);
}

// MSL 只提供 rotate(), 这里显式写出左右循环移位, 免得方向与位数的语义含糊.
static inline uint rotl32(uint x, uint n) {
    return (x << n) | (x >> ((32u - n) & 31u));
}

static inline uint rotr32(uint x, uint n) {
    return (x >> n) | (x << ((32u - n) & 31u));
}

// 把 seed 的十进制形式写到 out 的开头, 返回位数.
//
// 两个全展开的循环都只用常量下标, 中间结果尽量留在寄存器里,
// 只有最后一次按倒序落到 out 时才是动态下标.
static inline uint write_decimal(thread uchar* out, ulong value) {
    uchar tmp[20];
    if (value < 0x100000000UL) {
        // 常见情形: 搜索范围在 32 位以内, 用 32 位除法即可.
        uint x = (uint)value;
        #pragma unroll
        for (uint i = 0; i < 10; ++i) {
            tmp[i] = (uchar)('0' + (x % 10u));
            x /= 10u;
        }
        #pragma unroll
        for (uint i = 10; i < 20; ++i) {
            tmp[i] = (uchar)'0';
        }
    } else {
        ulong x = value;
        #pragma unroll
        for (uint i = 0; i < 20; ++i) {
            tmp[i] = (uchar)('0' + (uchar)(x % 10UL));
            x /= 10UL;
        }
    }

    // 位数: 最高的那个非零位加一, 0 本身算一位.
    uint digits = 1u;
    #pragma unroll
    for (uint i = 0; i < 20; ++i) {
        if (tmp[i] != (uchar)'0' || i == 0u) {
            digits = i + 1u;
        }
    }

    #pragma unroll
    for (uint i = 0; i < 20; ++i) {
        if (i < digits) {
            out[digits - 1u - i] = tmp[i];
        }
    }
    return digits;
}

// ---------------------------------------------------------------------------
// MD5: 消息恒小于 56 字节, 只需要处理一个 64 字节块
// ---------------------------------------------------------------------------

#define MD5_F(x, y, z) (((x) & (y)) | (~(x) & (z)))
#define MD5_G(x, y, z) (((x) & (z)) | ((y) & ~(z)))
#define MD5_H(x, y, z) ((x) ^ (y) ^ (z))
#define MD5_I(x, y, z) ((y) ^ ((x) | ~(z)))
#define MD5_STEP(f, a, b, c, d, x, t, s)     \
    do {                                     \
        (a) += f((b), (c), (d)) + (x) + (t); \
        (a) = rotl32((a), (s));              \
        (a) += (b);                          \
    } while (0)

// 对已经补好位的 64 字节块做一次压缩, state 是四个链接变量.
static inline uint4 md5_compress(thread const uchar* block, uint4 state) {
    uint m[16];
    #pragma unroll
    for (uint i = 0; i < 16; ++i) {
        m[i] = load_le32_thread(block + 4u * i);
    }

    uint a = state.x;
    uint b = state.y;
    uint c = state.z;
    uint d = state.w;

    // 第一轮: F, 消息序号 0..15
    MD5_STEP(MD5_F, a, b, c, d, m[0], MD5_K[0], 7);
    MD5_STEP(MD5_F, d, a, b, c, m[1], MD5_K[1], 12);
    MD5_STEP(MD5_F, c, d, a, b, m[2], MD5_K[2], 17);
    MD5_STEP(MD5_F, b, c, d, a, m[3], MD5_K[3], 22);
    MD5_STEP(MD5_F, a, b, c, d, m[4], MD5_K[4], 7);
    MD5_STEP(MD5_F, d, a, b, c, m[5], MD5_K[5], 12);
    MD5_STEP(MD5_F, c, d, a, b, m[6], MD5_K[6], 17);
    MD5_STEP(MD5_F, b, c, d, a, m[7], MD5_K[7], 22);
    MD5_STEP(MD5_F, a, b, c, d, m[8], MD5_K[8], 7);
    MD5_STEP(MD5_F, d, a, b, c, m[9], MD5_K[9], 12);
    MD5_STEP(MD5_F, c, d, a, b, m[10], MD5_K[10], 17);
    MD5_STEP(MD5_F, b, c, d, a, m[11], MD5_K[11], 22);
    MD5_STEP(MD5_F, a, b, c, d, m[12], MD5_K[12], 7);
    MD5_STEP(MD5_F, d, a, b, c, m[13], MD5_K[13], 12);
    MD5_STEP(MD5_F, c, d, a, b, m[14], MD5_K[14], 17);
    MD5_STEP(MD5_F, b, c, d, a, m[15], MD5_K[15], 22);

    // 第二轮: G, 消息序号 (1 + 5 * i) mod 16
    MD5_STEP(MD5_G, a, b, c, d, m[1], MD5_K[16], 5);
    MD5_STEP(MD5_G, d, a, b, c, m[6], MD5_K[17], 9);
    MD5_STEP(MD5_G, c, d, a, b, m[11], MD5_K[18], 14);
    MD5_STEP(MD5_G, b, c, d, a, m[0], MD5_K[19], 20);
    MD5_STEP(MD5_G, a, b, c, d, m[5], MD5_K[20], 5);
    MD5_STEP(MD5_G, d, a, b, c, m[10], MD5_K[21], 9);
    MD5_STEP(MD5_G, c, d, a, b, m[15], MD5_K[22], 14);
    MD5_STEP(MD5_G, b, c, d, a, m[4], MD5_K[23], 20);
    MD5_STEP(MD5_G, a, b, c, d, m[9], MD5_K[24], 5);
    MD5_STEP(MD5_G, d, a, b, c, m[14], MD5_K[25], 9);
    MD5_STEP(MD5_G, c, d, a, b, m[3], MD5_K[26], 14);
    MD5_STEP(MD5_G, b, c, d, a, m[8], MD5_K[27], 20);
    MD5_STEP(MD5_G, a, b, c, d, m[13], MD5_K[28], 5);
    MD5_STEP(MD5_G, d, a, b, c, m[2], MD5_K[29], 9);
    MD5_STEP(MD5_G, c, d, a, b, m[7], MD5_K[30], 14);
    MD5_STEP(MD5_G, b, c, d, a, m[12], MD5_K[31], 20);

    // 第三轮: H, 消息序号 (5 + 3 * i) mod 16
    MD5_STEP(MD5_H, a, b, c, d, m[5], MD5_K[32], 4);
    MD5_STEP(MD5_H, d, a, b, c, m[8], MD5_K[33], 11);
    MD5_STEP(MD5_H, c, d, a, b, m[11], MD5_K[34], 16);
    MD5_STEP(MD5_H, b, c, d, a, m[14], MD5_K[35], 23);
    MD5_STEP(MD5_H, a, b, c, d, m[1], MD5_K[36], 4);
    MD5_STEP(MD5_H, d, a, b, c, m[4], MD5_K[37], 11);
    MD5_STEP(MD5_H, c, d, a, b, m[7], MD5_K[38], 16);
    MD5_STEP(MD5_H, b, c, d, a, m[10], MD5_K[39], 23);
    MD5_STEP(MD5_H, a, b, c, d, m[13], MD5_K[40], 4);
    MD5_STEP(MD5_H, d, a, b, c, m[0], MD5_K[41], 11);
    MD5_STEP(MD5_H, c, d, a, b, m[3], MD5_K[42], 16);
    MD5_STEP(MD5_H, b, c, d, a, m[6], MD5_K[43], 23);
    MD5_STEP(MD5_H, a, b, c, d, m[9], MD5_K[44], 4);
    MD5_STEP(MD5_H, d, a, b, c, m[12], MD5_K[45], 11);
    MD5_STEP(MD5_H, c, d, a, b, m[15], MD5_K[46], 16);
    MD5_STEP(MD5_H, b, c, d, a, m[2], MD5_K[47], 23);

    // 第四轮: I, 消息序号 (7 * i) mod 16
    MD5_STEP(MD5_I, a, b, c, d, m[0], MD5_K[48], 6);
    MD5_STEP(MD5_I, d, a, b, c, m[7], MD5_K[49], 10);
    MD5_STEP(MD5_I, c, d, a, b, m[14], MD5_K[50], 15);
    MD5_STEP(MD5_I, b, c, d, a, m[5], MD5_K[51], 21);
    MD5_STEP(MD5_I, a, b, c, d, m[12], MD5_K[52], 6);
    MD5_STEP(MD5_I, d, a, b, c, m[3], MD5_K[53], 10);
    MD5_STEP(MD5_I, c, d, a, b, m[10], MD5_K[54], 15);
    MD5_STEP(MD5_I, b, c, d, a, m[1], MD5_K[55], 21);
    MD5_STEP(MD5_I, a, b, c, d, m[8], MD5_K[56], 6);
    MD5_STEP(MD5_I, d, a, b, c, m[15], MD5_K[57], 10);
    MD5_STEP(MD5_I, c, d, a, b, m[6], MD5_K[58], 15);
    MD5_STEP(MD5_I, b, c, d, a, m[13], MD5_K[59], 21);
    MD5_STEP(MD5_I, a, b, c, d, m[4], MD5_K[60], 6);
    MD5_STEP(MD5_I, d, a, b, c, m[11], MD5_K[61], 10);
    MD5_STEP(MD5_I, c, d, a, b, m[2], MD5_K[62], 15);
    MD5_STEP(MD5_I, b, c, d, a, m[9], MD5_K[63], 21);

    return uint4(state.x + a, state.y + b, state.z + c, state.w + d);
}

// ---------------------------------------------------------------------------
// AES-128 解密: 只需要密钥扩展加 10 轮 InvCipher
// ---------------------------------------------------------------------------

// 状态按列存成 4 个 32 位字, 第 k 个字的第 r 个字节就是 state[r][k].
// 这样密文按小端装字即可, 轮密钥也天然同构, 无需转置.
struct AesState {
    uint s0;
    uint s1;
    uint s2;
    uint s3;
};

// 单字节 xtime, 对 4 个字节并行做.
static inline uint xtime_bytes(uint x) {
    return ((x << 1) & 0xfefefefeu) ^ (((x >> 7) & 0x01010101u) * 0x1bu);
}

// 字内每个字节各查一次 S 盒, 用于密钥扩展的 SubWord.
static inline uint sub_word(uint x) {
    uchar4 b = as_type<uchar4>(x);
    return as_type<uint>(uchar4(AES_SBOX[b.x], AES_SBOX[b.y], AES_SBOX[b.z], AES_SBOX[b.w]));
}

// 字内每个字节各查一次逆 S 盒.
static inline uint inv_sub_word(uint x) {
    uchar4 b = as_type<uchar4>(x);
    return as_type<uint>(
        uchar4(AES_INV_SBOX[b.x], AES_INV_SBOX[b.y], AES_INV_SBOX[b.z], AES_INV_SBOX[b.w]));
}

// InvShiftRows: 第 r 行循环右移 r 个字节, 也就是新列 j 的第 r 字节取自旧列 (j - r) mod 4.
static inline AesState inv_shift_rows(AesState st) {
    AesState r;
    r.s0 = (st.s0 & 0x000000ffu) | (st.s3 & 0x0000ff00u) | (st.s2 & 0x00ff0000u) |
           (st.s1 & 0xff000000u);
    r.s1 = (st.s1 & 0x000000ffu) | (st.s0 & 0x0000ff00u) | (st.s3 & 0x00ff0000u) |
           (st.s2 & 0xff000000u);
    r.s2 = (st.s2 & 0x000000ffu) | (st.s1 & 0x0000ff00u) | (st.s0 & 0x00ff0000u) |
           (st.s3 & 0xff000000u);
    r.s3 = (st.s3 & 0x000000ffu) | (st.s2 & 0x0000ff00u) | (st.s1 & 0x00ff0000u) |
           (st.s0 & 0xff000000u);
    return r;
}

// InvMixColumns 的每一列: b_i = a_i ^ 2 * (a_i ^ a_i+1) ^ 4 * (a_i ^ a_i+2) ^ 9 * s,
// 其中 s 是该列四个字节的异或和. 这是逆矩阵乘法的等价变形, 比逐项查表省很多指令.
static inline uint inv_mix_column(uint a) {
    uint r8 = rotr32(a, 8);
    uint r16 = rotr32(a, 16);
    uint r24 = rotr32(a, 24);
    uint sum = (a ^ r8 ^ r16 ^ r24) & 0xffu;
    uint m9 = (uint)AES_MUL9[sum] * 0x01010101u;
    uint t2 = xtime_bytes(a ^ r8);
    uint t3 = xtime_bytes(xtime_bytes(a ^ r16));
    return a ^ t2 ^ t3 ^ m9;
}

// 解密一个 16 字节块, key_words 是 16 字节密钥按小端装成的 4 个字.
static inline uint4 aes128_decrypt_block(uint4 key_words, uint4 input) {
    // 密钥扩展: 44 个字, 第 r 轮的轮密钥是 w[4r .. 4r + 3].
    uint w[44];
    w[0] = key_words.x;
    w[1] = key_words.y;
    w[2] = key_words.z;
    w[3] = key_words.w;
    #pragma unroll
    for (uint i = 4; i < 44; ++i) {
        uint t = w[i - 1u];
        if ((i & 3u) == 0u) {
            // 字内字节序是反的, 所以 RotWord 在这里等价于循环右移 8 位.
            t = sub_word(rotr32(t, 8)) ^ (uint)AES_RCON[i / 4u - 1u];
        }
        w[i] = w[i - 4u] ^ t;
    }

    AesState st;
    st.s0 = input.x;
    st.s1 = input.y;
    st.s2 = input.z;
    st.s3 = input.w;

    st.s0 ^= w[40];
    st.s1 ^= w[41];
    st.s2 ^= w[42];
    st.s3 ^= w[43];

    #pragma unroll
    for (int round = 9; round >= 1; --round) {
        st = inv_shift_rows(st);
        st.s0 = inv_sub_word(st.s0);
        st.s1 = inv_sub_word(st.s1);
        st.s2 = inv_sub_word(st.s2);
        st.s3 = inv_sub_word(st.s3);
        st.s0 ^= w[4 * round];
        st.s1 ^= w[4 * round + 1];
        st.s2 ^= w[4 * round + 2];
        st.s3 ^= w[4 * round + 3];
        st.s0 = inv_mix_column(st.s0);
        st.s1 = inv_mix_column(st.s1);
        st.s2 = inv_mix_column(st.s2);
        st.s3 = inv_mix_column(st.s3);
    }

    st = inv_shift_rows(st);
    st.s0 = inv_sub_word(st.s0);
    st.s1 = inv_sub_word(st.s1);
    st.s2 = inv_sub_word(st.s2);
    st.s3 = inv_sub_word(st.s3);
    st.s0 ^= w[0];
    st.s1 ^= w[1];
    st.s2 ^= w[2];
    st.s3 ^= w[3];

    return uint4(st.s0, st.s1, st.s2, st.s3);
}

// ---------------------------------------------------------------------------
// 魔数判定: 与 CPU 侧 crypto::match_magic 覆盖同样的格式
// ---------------------------------------------------------------------------

// 明文块按四个 32 位字传入, 每个字自低到高对应块内的连续 4 个字节.
//
// MSL 里 uchar16 是保留类型名, 不能直接拿来当向量用, 所以按字拆开比对.
static inline bool magic_match(uint4 words) {
    uchar4 w0 = as_type<uchar4>(words.x);
    uchar4 w1 = as_type<uchar4>(words.y);
    uchar4 w2 = as_type<uchar4>(words.z);

    // PNG
    if (w0[0] == 0x89 && w0[1] == 'P' && w0[2] == 'N' && w0[3] == 'G' && w1[0] == 0x0d &&
        w1[1] == 0x0a && w1[2] == 0x1a && w1[3] == 0x0a) {
        return true;
    }
    // GIF87a / GIF89a
    if (w0[0] == 'G' && w0[1] == 'I' && w0[2] == 'F' && w0[3] == '8' &&
        (w1[0] == '7' || w1[0] == '9') && w1[1] == 'a') {
        return true;
    }
    // JPEG
    if (w0[0] == 0xff && w0[1] == 0xd8 && w0[2] == 0xff) {
        return true;
    }
    // 微信自己的 wxgf 容器
    if (w0[0] == 'w' && w0[1] == 'x' && w0[2] == 'g' && w0[3] == 'f') {
        return true;
    }
    // WebP: RIFF + 4 字节长度 + WEBP
    if (w0[0] == 'R' && w0[1] == 'I' && w0[2] == 'F' && w0[3] == 'F' && w2[0] == 'W' &&
        w2[1] == 'E' && w2[2] == 'B' && w2[3] == 'P') {
        return true;
    }
    return false;
}

// ---------------------------------------------------------------------------
// 主 kernel
// ---------------------------------------------------------------------------

// 派生单个候选的密钥: 拼消息 + 单块 MD5.
static inline uint4 derive_key_words(
    device const uchar* wxid_data,
    constant WxidEntry* wxid_table,
    uint wxid_index,
    ulong seed)
{
    thread uchar msg[64];
    uint len = write_decimal(msg, seed);
    WxidEntry entry = wxid_table[wxid_index];
    for (uint i = 0; i < entry.length; ++i) {
        msg[len + i] = wxid_data[entry.offset + i];
    }
    len += entry.length;
    #pragma unroll
    for (uint i = 0; i < 8; ++i) {
        msg[len + i] = MSG_SUFFIX[i];
    }
    len += 8u;

    // 补位: 0x80, 补零到 56 字节, 末尾 8 字节是消息比特长度.
    msg[len] = 0x80u;
    for (uint i = len + 1u; i < 56u; ++i) {
        msg[i] = 0u;
    }
    ulong bits = (ulong)len * 8UL;
    #pragma unroll
    for (uint i = 0; i < 8; ++i) {
        msg[56u + i] = (uchar)(bits >> (8u * i));
    }

    return md5_compress(msg, uint4(0x67452301u, 0xefcdab89u, 0x98badcfeu, 0x10325476u));
}

kernel void seed_search(
    device const uchar* wxid_data [[buffer(0)]],
    constant WxidEntry* wxid_table [[buffer(1)]],
    device const uchar* sample_prefixes [[buffer(2)]],
    constant SeedParams& params [[buffer(3)]],
    device ulong* hits [[buffer(4)]],
    device atomic_uint* hit_count [[buffer(5)]],
    device atomic_uint* stop_flag [[buffer(6)]],
    uint gid [[thread_position_in_grid]])
{
    uint total = params.count * params.wxid_count;
    if (gid >= total) {
        return;
    }
    // 主机已经决定提前退出时, 在途的批次直接放空.
    if (atomic_load_explicit(stop_flag, memory_order_relaxed) != 0u) {
        return;
    }

    uint wxid_index = gid % params.wxid_count;
    ulong seed = params.base + (ulong)(gid / params.wxid_count);

    // MD5 的前 16 字节就是密钥, 正好是四个链接变量按小端展开.
    uint4 key = derive_key_words(wxid_data, wxid_table, wxid_index, seed);

    // 逐个样本判定首块, 第一个不匹配就丢弃, 避免无谓的 AES.
    bool all_matched = true;
    for (uint s = 0; s < params.sample_count; ++s) {
        uint4 cipher = uint4(
            load_le32_device(sample_prefixes + 16u * s),
            load_le32_device(sample_prefixes + 16u * s + 4u),
            load_le32_device(sample_prefixes + 16u * s + 8u),
            load_le32_device(sample_prefixes + 16u * s + 12u));
        // CBC 的首块解密后再与 IV 异或, 这里的 IV 就是密钥本身.
        uint4 plain = aes128_decrypt_block(key, cipher) ^ key;
        if (!magic_match(plain)) {
            all_matched = false;
            break;
        }
    }

    if (all_matched) {
        uint slot = atomic_fetch_add_explicit(hit_count, 1u, memory_order_relaxed);
        if (slot < params.max_hits) {
            hits[slot] = seed;
        }
        // 通知主机以及尚未执行的批次提前退出.
        atomic_store_explicit(stop_flag, 1u, memory_order_relaxed);
    }
}

// 只做消息拼接与 MD5, 把每个候选的密钥原样写出来.
// 主机侧用它逐字节核对 MSL 里的 MD5 是否与 Rust 的 md-5 一致.
kernel void derive_keys(
    device const uchar* wxid_data [[buffer(0)]],
    constant WxidEntry* wxid_table [[buffer(1)]],
    constant SeedParams& params [[buffer(3)]],
    device uchar* keys [[buffer(4)]],
    uint gid [[thread_position_in_grid]])
{
    if (gid >= params.count) {
        return;
    }
    ulong seed = params.base + (ulong)gid;
    uint4 key = derive_key_words(wxid_data, wxid_table, 0u, seed);
    device uchar* out = keys + 16u * gid;
    #pragma unroll
    for (uint i = 0; i < 4; ++i) {
        out[4u * i] = (uchar)(key[i] & 0xffu);
        out[4u * i + 1u] = (uchar)((key[i] >> 8) & 0xffu);
        out[4u * i + 2u] = (uchar)((key[i] >> 16) & 0xffu);
        out[4u * i + 3u] = (uchar)((key[i] >> 24) & 0xffu);
    }
}
