// wxemo CUDA 搜索内核.
//
// 每个线程负责一个候选 seed, 顺序为:
//   1. 拼消息 "<seed 十进制>" + wxid + "EMOTICON", 补成单个 64 字节 MD5 块;
//   2. 在核内做单块 MD5, 摘要前 16 字节就是候选密钥;
//   3. 用该密钥做完整 AES-128 密钥扩展, 解出每个样本的密文首块,
//      再与密钥本身异或 (CBC 的 IV 等于密钥);
//   4. 首块开头命中已知图片魔数才算候选, 第一个样本不命中就丢弃.
//
// 镜像必须是 cubin (SASS) 而不是 PTX: CUDA 13.4 产出的 PTX 9.4 高于当前驱动
// (CUDA 13.0) 支持的版本, cuModuleLoadData 会直接返回 CUDA_ERROR_UNSUPPORTED_PTX_VERSION.

#include <cuda_runtime.h>

// 单个 wxid 的最大字节数. 消息必须放进一个 MD5 块 (不超过 55 字节),
// host 侧会在启动前校验这一点.
#define WXEMO_WXID_MAX 32
#define WXEMO_WXID_COUNT_MAX 4
#define WXEMO_SAMPLE_MAX 16

// ---------------------------------------------------------------- 常量内存

// 全部候选共用的输入, 每次搜索前由 host 通过 cuMemcpyHtoD 填一次.
extern "C" __constant__ unsigned char g_wxid[WXEMO_WXID_COUNT_MAX][WXEMO_WXID_MAX];
extern "C" __constant__ unsigned int g_wxid_len[WXEMO_WXID_COUNT_MAX];
extern "C" __constant__ unsigned int g_wxid_count;
extern "C" __constant__ unsigned int g_sample_count;
extern "C" __constant__ unsigned char g_prefix[WXEMO_SAMPLE_MAX][16];

// AES S 盒与逆 S 盒. 内核启动时搬到共享内存再使用, 因为线程之间访问的下标是发散的,
// 直接读常量缓存会被逐地址串行化.
extern "C" __constant__ unsigned char g_sbox[256] = {
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

extern "C" __constant__ unsigned char g_inv_sbox[256] = {
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

// MD5 每轮的加法常量 T[i] = floor(2^32 * abs(sin(i + 1))).
__device__ __constant__ unsigned int g_md5_k[64] = {
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
__device__ __constant__ unsigned int g_md5_s[64] = {
    7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22,
    5, 9, 14, 20, 5, 9, 14, 20, 5, 9, 14, 20, 5, 9, 14, 20,
    4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23,
    6, 10, 15, 21, 6, 10, 15, 21, 6, 10, 15, 21, 6, 10, 15, 21,
};

// AES 密钥扩展的轮常量, 已经左移到字的高字节.
__device__ __constant__ unsigned int g_rcon[10] = {
    0x01000000u, 0x02000000u, 0x04000000u, 0x08000000u, 0x10000000u,
    0x20000000u, 0x40000000u, 0x80000000u, 0x1b000000u, 0x36000000u,
};

// ---------------------------------------------------------------- 基础工具

__device__ __forceinline__ unsigned int rotl32(unsigned int value, unsigned int shift) {
    // 单个 __funnelshift_l 就是一条 SHF, 比 (value << n) | (value >> (32 - n)) 少一条指令.
    return __funnelshift_l(value, value, shift);
}

// ---------------------------------------------------------------- MD5

// 把一个已经补好填充的 64 字节块压成 16 字节摘要.
//
// 消息恒短于 56 字节, 所以只需要一个块, 不需要分块循环和长度累加器.
__device__ __forceinline__ void md5_block(const unsigned char block[64], unsigned char digest[16]) {
    unsigned int words[16];
#pragma unroll
    for (int i = 0; i < 16; ++i) {
        words[i] = (unsigned int)block[4 * i]
                 | ((unsigned int)block[4 * i + 1] << 8)
                 | ((unsigned int)block[4 * i + 2] << 16)
                 | ((unsigned int)block[4 * i + 3] << 24);
    }

    unsigned int a = 0x67452301u;
    unsigned int b = 0xefcdab89u;
    unsigned int c = 0x98badcfeu;
    unsigned int d = 0x10325476u;

#pragma unroll
    for (int i = 0; i < 64; ++i) {
        // i 是编译期常量, 展开后 g 也随之成为常量下标.
        unsigned int f;
        unsigned int g;
        if (i < 16) {
            f = (b & c) | (~b & d);
            g = (unsigned int)i;
        } else if (i < 32) {
            f = (d & b) | (~d & c);
            g = (5u * (unsigned int)i + 1u) & 15u;
        } else if (i < 48) {
            f = b ^ c ^ d;
            g = (3u * (unsigned int)i + 5u) & 15u;
        } else {
            f = c ^ (b | ~d);
            g = (7u * (unsigned int)i) & 15u;
        }
        unsigned int previous_d = d;
        d = c;
        c = b;
        b = b + rotl32(a + f + g_md5_k[i] + words[g], g_md5_s[i]);
        a = previous_d;
    }

    a += 0x67452301u;
    b += 0xefcdab89u;
    c += 0x98badcfeu;
    d += 0x10325476u;

    // 摘要按小端写出.
    digest[0] = (unsigned char)a;
    digest[1] = (unsigned char)(a >> 8);
    digest[2] = (unsigned char)(a >> 16);
    digest[3] = (unsigned char)(a >> 24);
    digest[4] = (unsigned char)b;
    digest[5] = (unsigned char)(b >> 8);
    digest[6] = (unsigned char)(b >> 16);
    digest[7] = (unsigned char)(b >> 24);
    digest[8] = (unsigned char)c;
    digest[9] = (unsigned char)(c >> 8);
    digest[10] = (unsigned char)(c >> 16);
    digest[11] = (unsigned char)(c >> 24);
    digest[12] = (unsigned char)d;
    digest[13] = (unsigned char)(d >> 8);
    digest[14] = (unsigned char)(d >> 16);
    digest[15] = (unsigned char)(d >> 24);
}

// 构造单个 MD5 块: "<seed 十进制>" + wxid + "EMOTICON", 再补零, 写 0x80 与 64 位长度.
//
// 返回消息本身的字节数. 调用方保证它不超过 55.
__device__ __forceinline__ unsigned int build_block(
    unsigned char block[64],
    unsigned long long seed,
    const unsigned char* wxid,
    unsigned int wxid_len,
    unsigned int suffix_len) {
    // 十进制 seed, 先低位后高位地取出来, 再倒序写入.
    unsigned char digits[20];
    unsigned int count = 0;
    unsigned long long rest = seed;
    do {
        digits[count] = (unsigned char)('0' + (unsigned int)(rest % 10ull));
        rest /= 10ull;
        ++count;
    } while (rest != 0ull);

    unsigned int length = 0;
    for (unsigned int i = 0; i < count; ++i) {
        block[length] = digits[count - 1 - i];
        ++length;
    }
    for (unsigned int i = 0; i < wxid_len; ++i) {
        block[length] = wxid[i];
        ++length;
    }
    const unsigned char suffix[8] = {'E', 'M', 'O', 'T', 'I', 'C', 'O', 'N'};
    for (unsigned int i = 0; i < suffix_len; ++i) {
        block[length] = suffix[i];
        ++length;
    }

    const unsigned int message_len = length;
    block[length] = 0x80u;
    ++length;
    for (unsigned int i = length; i < 56; ++i) {
        block[i] = 0u;
    }
    unsigned long long bits = (unsigned long long)message_len * 8ull;
#pragma unroll
    for (int i = 0; i < 8; ++i) {
        block[56 + i] = (unsigned char)(bits >> (8 * i));
    }
    return message_len;
}

// ---------------------------------------------------------------- AES-128

__device__ __forceinline__ unsigned int load_be32(const unsigned char* p) {
    return ((unsigned int)p[0] << 24) | ((unsigned int)p[1] << 16)
         | ((unsigned int)p[2] << 8) | (unsigned int)p[3];
}

__device__ __forceinline__ unsigned int sub_word(const unsigned char* sbox, unsigned int word) {
    return ((unsigned int)sbox[(word >> 24) & 0xffu] << 24)
         | ((unsigned int)sbox[(word >> 16) & 0xffu] << 16)
         | ((unsigned int)sbox[(word >> 8) & 0xffu] << 8)
         | (unsigned int)sbox[word & 0xffu];
}

// 标准 AES-128 密钥扩展, 展开成 44 个字 (11 组轮密钥).
// 全部下标在展开后都是编译期常量, 所以 44 个字留在寄存器里.
__device__ __forceinline__ void aes128_expand(
    const unsigned char* sbox,
    const unsigned char key[16],
    unsigned int round_keys[44]) {
#pragma unroll
    for (int i = 0; i < 4; ++i) {
        round_keys[i] = load_be32(key + 4 * i);
    }
#pragma unroll
    for (int i = 4; i < 44; ++i) {
        unsigned int temp = round_keys[i - 1];
        if ((i & 3) == 0) {
            // RotWord 就是循环左移 8 位, 再 SubWord 并异或轮常量.
            temp = sub_word(sbox, rotl32(temp, 8)) ^ g_rcon[i / 4 - 1];
        }
        round_keys[i] = round_keys[i - 4] ^ temp;
    }
}

__device__ __forceinline__ unsigned char xtime(unsigned char value) {
    // GF(2^8) 上乘 2, 模 x^8 + x^4 + x^3 + x + 1 (0x11b).
    return (unsigned char)((value << 1) ^ ((unsigned char)((value >> 7) * 0x1bu)));
}

// InvMixColumns 需要的四个固定乘数. 每个都用 xtime 展开成 8a / 4a / 2a 的组合,
// 不用通用 GF 乘法. 8a = v, 4a = u, 2a = t.
__device__ __forceinline__ unsigned char mul9(unsigned char a) {
    unsigned char t = xtime(a);
    unsigned char u = xtime(t);
    unsigned char v = xtime(u);
    return (unsigned char)(v ^ a);
}

__device__ __forceinline__ unsigned char mul11(unsigned char a) {
    unsigned char t = xtime(a);
    unsigned char u = xtime(t);
    unsigned char v = xtime(u);
    return (unsigned char)(v ^ t ^ a);
}

__device__ __forceinline__ unsigned char mul13(unsigned char a) {
    unsigned char t = xtime(a);
    unsigned char u = xtime(t);
    unsigned char v = xtime(u);
    return (unsigned char)(v ^ u ^ a);
}

__device__ __forceinline__ unsigned char mul14(unsigned char a) {
    unsigned char t = xtime(a);
    unsigned char u = xtime(t);
    unsigned char v = xtime(u);
    return (unsigned char)(v ^ u ^ t);
}

// InvShiftRows: 第 r 行循环右移 r 个字节. 状态按列优先线性存放,
// s[4 * col + row] 就是第 row 行第 col 列.
__device__ __forceinline__ void inv_shift_rows(unsigned char s[16]) {
    unsigned char temp;
    // 第 1 行下标 1, 5, 9, 13
    temp = s[13];
    s[13] = s[9];
    s[9] = s[5];
    s[5] = s[1];
    s[1] = temp;
    // 第 2 行下标 2, 6, 10, 14
    temp = s[2];
    s[2] = s[10];
    s[10] = temp;
    temp = s[6];
    s[6] = s[14];
    s[14] = temp;
    // 第 3 行下标 3, 7, 11, 15, 右移 3 等价于左移 1
    temp = s[3];
    s[3] = s[7];
    s[7] = s[11];
    s[11] = s[15];
    s[15] = temp;
}

__device__ __forceinline__ void inv_sub_bytes(const unsigned char* inv_sbox, unsigned char s[16]) {
#pragma unroll
    for (int i = 0; i < 16; ++i) {
        s[i] = inv_sbox[s[i]];
    }
}

__device__ __forceinline__ void add_round_key(
    unsigned char s[16],
    const unsigned int round_keys[44],
    int round) {
#pragma unroll
    for (int col = 0; col < 4; ++col) {
        unsigned int word = round_keys[4 * round + col];
        s[4 * col + 0] ^= (unsigned char)(word >> 24);
        s[4 * col + 1] ^= (unsigned char)(word >> 16);
        s[4 * col + 2] ^= (unsigned char)(word >> 8);
        s[4 * col + 3] ^= (unsigned char)word;
    }
}

__device__ __forceinline__ void inv_mix_columns(unsigned char s[16]) {
#pragma unroll
    for (int col = 0; col < 4; ++col) {
        unsigned char a0 = s[4 * col + 0];
        unsigned char a1 = s[4 * col + 1];
        unsigned char a2 = s[4 * col + 2];
        unsigned char a3 = s[4 * col + 3];
        s[4 * col + 0] = (unsigned char)(mul14(a0) ^ mul11(a1) ^ mul13(a2) ^ mul9(a3));
        s[4 * col + 1] = (unsigned char)(mul9(a0) ^ mul14(a1) ^ mul11(a2) ^ mul13(a3));
        s[4 * col + 2] = (unsigned char)(mul13(a0) ^ mul9(a1) ^ mul14(a2) ^ mul11(a3));
        s[4 * col + 3] = (unsigned char)(mul11(a0) ^ mul13(a1) ^ mul9(a2) ^ mul14(a3));
    }
}

// 用已经扩展好的轮密钥解密单个 16 字节块.
//
// 这里用的是教科书 InvCipher 顺序 (InvShiftRows -> InvSubBytes -> AddRoundKey
// -> InvMixColumns), 轮密钥按正序扩展, 倒序使用, 不需要额外的等价变换.
//
// 试过改用等价逆密码的 T 表写法, 把一轮压成 16 次查表加 12 次异或, 也试过配套的
// __launch_bounds__(256, 2) 与 (256, 4) 两档寄存器预算. 实测在 RTX 4060 上扫
// 2^30 个候选反而从 1.43 秒退到 1.56 秒: 四个 1 KB 的表让每轮 16 次查表的下标在
// 32 个 bank 上摊得更散, 省下来的 GF 乘法抵不过多出来的共享内存冲突与延迟.
// 所以保留查表规模小得多的逐字节写法.
__device__ __forceinline__ void aes128_decrypt_block(
    const unsigned char* inv_sbox,
    const unsigned int round_keys[44],
    const unsigned char input[16],
    unsigned char output[16]) {
    unsigned char s[16];
#pragma unroll
    for (int i = 0; i < 16; ++i) {
        s[i] = input[i];
    }

    add_round_key(s, round_keys, 10);
#pragma unroll
    for (int round = 9; round >= 1; --round) {
        inv_shift_rows(s);
        inv_sub_bytes(inv_sbox, s);
        add_round_key(s, round_keys, round);
        inv_mix_columns(s);
    }
    inv_shift_rows(s);
    inv_sub_bytes(inv_sbox, s);
    add_round_key(s, round_keys, 0);

#pragma unroll
    for (int i = 0; i < 16; ++i) {
        output[i] = s[i];
    }
}

// ---------------------------------------------------------------- 魔数判定

// 明文首块命中的魔数强度 (字节数), 0 表示不命中.
//
// 与 CPU 侧 match_magic 的顺序和取值保持一致. 所有分支的强度都不小于 3,
// 所以 CPU 侧的 MIN_STRENGTH 过滤在这里天然成立.
__device__ __forceinline__ unsigned int magic_strength(const unsigned char* block) {
    if (block[0] == 0x89u && block[1] == 'P' && block[2] == 'N' && block[3] == 'G'
        && block[4] == '\r' && block[5] == '\n' && block[6] == 0x1au && block[7] == '\n') {
        return 8u;
    }
    if (block[0] == 'G' && block[1] == 'I' && block[2] == 'F' && block[3] == '8'
        && (block[4] == '7' || block[4] == '9') && block[5] == 'a') {
        return 6u;
    }
    if (block[0] == 0xffu && block[1] == 0xd8u && block[2] == 0xffu) {
        return 3u;
    }
    if (block[0] == 'w' && block[1] == 'x' && block[2] == 'g' && block[3] == 'f') {
        return 4u;
    }
    if (block[0] == 'R' && block[1] == 'I' && block[2] == 'F' && block[3] == 'F'
        && block[8] == 'W' && block[9] == 'E' && block[10] == 'B' && block[11] == 'P') {
        return 12u;
    }
    return 0u;
}

// ---------------------------------------------------------------- 搜索内核

extern "C" __global__ void __launch_bounds__(256) wxemo_search_kernel(
    unsigned long long batch_start,
    unsigned long long batch_count,
    unsigned long long limit,
    unsigned long long* out_seed,
    unsigned int* out_wxid,
    unsigned int* out_flag) {
    // S 盒搬到共享内存: 线程之间的下标是发散的, 常量缓存会把它们逐地址串行化.
    __shared__ unsigned char shared_sbox[256];
    __shared__ unsigned char shared_inv_sbox[256];
    for (unsigned int i = threadIdx.x; i < 256u; i += blockDim.x) {
        shared_sbox[i] = g_sbox[i];
        shared_inv_sbox[i] = g_inv_sbox[i];
    }
    __syncthreads();

    unsigned long long index = (unsigned long long)blockIdx.x * (unsigned long long)blockDim.x
                             + (unsigned long long)threadIdx.x;
    if (index >= batch_count) {
        return;
    }
    unsigned long long seed = batch_start + index;
    if (seed >= limit) {
        return;
    }
    // 命中后主机不会立刻停掉本批, 这个标志只是让后面的线程少做无用功.
    if (*out_flag != 0u) {
        return;
    }

    unsigned char block[64];
    unsigned char key[16];
    unsigned char plain[16];

    for (unsigned int which = 0; which < g_wxid_count; ++which) {
        unsigned int wxid_len = g_wxid_len[which];
        unsigned int message_len = build_block(block, seed, g_wxid[which], wxid_len, 8u);
        if (message_len > 55u) {
            // host 侧已经校验过, 这里只做防御, 不猜第二个块.
            return;
        }
        md5_block(block, key);

        // 密钥扩展只做一次, 供同一候选的全部样本复用.
        unsigned int round_keys[44];
        aes128_expand(shared_sbox, key, round_keys);

        unsigned int all_matched = 1u;
        for (unsigned int sample = 0; sample < g_sample_count; ++sample) {
            aes128_decrypt_block(shared_inv_sbox, round_keys, g_prefix[sample], plain);
            // CBC 首块的 IV 就是密钥本身, 解密后再异或.
#pragma unroll
            for (int i = 0; i < 16; ++i) {
                plain[i] ^= key[i];
            }
            if (magic_strength(plain) == 0u) {
                all_matched = 0u;
                // 优先淘汰: 第一个样本不命中就不再算后面的样本.
                break;
            }
        }

        if (all_matched != 0u) {
            if (atomicCAS(out_flag, 0u, 1u) == 0u) {
                *out_seed = seed;
                *out_wxid = which;
            }
            return;
        }
    }
}
