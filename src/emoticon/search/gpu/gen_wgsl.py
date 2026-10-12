"""生成 wxemo GPU 内核的 WGSL 源码.

MD5 的 64 轮和 AES 的密钥扩展与 10 轮解密都按常量下标展开,
这样 naga / Metal 能把状态字留在寄存器里, 不需要动态下标的本地数组.

kernel.wgsl 是提交进仓库的产物, 本脚本只在需要改内核时运行:

    python3 gen_wgsl.py > kernel.wgsl
"""

import math

HEADER = """// wxemo seed 搜索的 wgpu 计算内核.
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
"""


def md5_k():
    return [int(abs(math.sin(i + 1)) * (1 << 32)) & 0xFFFFFFFF for i in range(64)]


def md5_s():
    out = []
    for shift in ([7, 12, 17, 22], [5, 9, 14, 20], [4, 11, 16, 23], [6, 10, 15, 21]):
        out.extend(shift * 4)
    return out


def md5_g():
    out = []
    for i in range(64):
        if i < 16:
            out.append(i)
        elif i < 32:
            out.append((5 * i + 1) % 16)
        elif i < 48:
            out.append((3 * i + 5) % 16)
        else:
            out.append((7 * i) % 16)
    return out


def md5_block():
    k = md5_k()
    s = md5_s()
    g = md5_g()
    lines = []
    lines.append("// 单块 MD5 摘要: 输入 16 个消息字, 返回 4 个摘要字.")
    lines.append("// 消息一定短于 56 字节, 所以压缩一轮即可, 返回前把初始向量加回去.")
    lines.append("fn md5_block(m: array<u32, 16>) -> array<u32, 4> {")
    lines.append("    var a = 0x67452301u;")
    lines.append("    var b = 0xefcdab89u;")
    lines.append("    var c = 0x98badcfeu;")
    lines.append("    var d = 0x10325476u;")
    lines.append("    var f = 0u;")
    lines.append("    var t = 0u;")
    lines.append("")
    names = [
        (0, "F = (b & c) | (~b & d)"),
        (1, "F = (d & b) | (~d & c)"),
        (2, "F = b ^ c ^ d"),
        (3, "F = c ^ (b | ~d)"),
    ]
    for group, comment in names:
        lines.append(f"    // 第 {group + 1} 组 16 轮, {comment}")
        for i in range(group * 16, group * 16 + 16):
            if group == 0:
                expr = "(b & c) | (~b & d)"
            elif group == 1:
                expr = "(d & b) | (~d & c)"
            elif group == 2:
                expr = "b ^ c ^ d"
            else:
                expr = "c ^ (b | ~d)"
            lines.append(f"    f = {expr};")
            lines.append(
                f"    t = a + f + m[{g[i]}] + 0x{k[i]:08x}u;"
            )
            lines.append(
                f"    a = d; d = c; c = b; b = b + rotl(t, {s[i]}u);"
            )
        lines.append("")
    lines.append("    return array<u32, 4>(")
    lines.append("        a + 0x67452301u,")
    lines.append("        b + 0xefcdab89u,")
    lines.append("        c + 0x98badcfeu,")
    lines.append("        d + 0x10325476u,")
    lines.append("    );")
    lines.append("}")
    return "\n".join(lines)


def gf_mul(a, b):
    p = 0
    for _ in range(8):
        if b & 1:
            p ^= a
        hi = a & 0x80
        a = (a << 1) & 0xFF
        if hi:
            a ^= 0x1B
        b >>= 1
    return p


def rotl8(x, n):
    return ((x << n) | (x >> (8 - n))) & 0xFF


def aes_sbox():
    box = []
    for i in range(256):
        inv = 0
        if i:
            for j in range(1, 256):
                if gf_mul(i, j) == 1:
                    inv = j
                    break
        x = inv
        box.append(x ^ rotl8(x, 1) ^ rotl8(x, 2) ^ rotl8(x, 3) ^ rotl8(x, 4) ^ 0x63)
    return box


def sbox_table():
    box = aes_sbox()
    assert box[0x00] == 0x63 and box[0x01] == 0x7C and box[0x53] == 0xED
    inv = [0] * 256
    for i, v in enumerate(box):
        inv[v] = i
    assert inv[0x63] == 0x00
    lines = ["// 前 256 项是 AES 正向 S 盒 (密钥扩展用), 后 256 项是逆 S 盒 (解密用)."]
    lines.append("// 由 aes_sbox() 生成, 已核对 S[0] = 0x63, S[1] = 0x7c, S[0x53] = 0xed.")
    vals = box + inv
    for start in range(0, 512, 8):
        chunk = ", ".join(f"0x{v:02x}u" for v in vals[start : start + 8])
        lines.append(f"//   {chunk}")
    return "\n".join(lines), box, inv


def aes_key_schedule():
    rcon = [0x01, 0x02, 0x04, 0x08, 0x10, 0x20, 0x40, 0x80, 0x1B, 0x36]
    lines = []
    lines.append("// AES-128 密钥扩展, 产生 44 个轮密钥字 (每个字是一列的 4 个字节, 大端序).")
    lines.append("fn aes_expand(k0: u32, k1: u32, k2: u32, k3: u32) -> array<u32, 44> {")
    lines.append("    var rk: array<u32, 44>;")
    lines.append("    rk[0] = k0;")
    lines.append("    rk[1] = k1;")
    lines.append("    rk[2] = k2;")
    lines.append("    rk[3] = k3;")
    for i in range(4, 44):
        if i % 4 == 0:
            lines.append(
                f"    rk[{i}] = rk[{i - 4}] ^ sub_word(rot_word(rk[{i - 1}])) ^ 0x{rcon[i // 4 - 1]:02x}000000u;"
            )
        else:
            lines.append(f"    rk[{i}] = rk[{i - 4}] ^ rk[{i - 1}];")
    lines.append("    return rk;")
    lines.append("}")
    return "\n".join(lines)


def inv_shift_rows():
    lines = []
    lines.append("// InvShiftRows: 第 r 行循环右移 r 个字节, 等价于新列 c 的第 r 行取自老列 (c - r) mod 4.")
    lines.append("fn inv_shift_rows(s: array<u32, 4>) -> array<u32, 4> {")
    lines.append("    let t0 = s[0];")
    lines.append("    let t1 = s[1];")
    lines.append("    let t2 = s[2];")
    lines.append("    let t3 = s[3];")
    lines.append("    // 每个字的高字节是第 0 行, 最低字节是第 3 行.")
    for c in range(4):
        src = [(c - r) % 4 for r in range(4)]
        parts = []
        for r in range(4):
            mask = 0xFF << (24 - 8 * r)
            parts.append(f"(t{src[r]} & 0x{mask:08x}u)")
        lines.append(f"    let o{c} = " + " | ".join(parts) + ";")
    lines.append("    return array<u32, 4>(o0, o1, o2, o3);")
    lines.append("}")
    return "\n".join(lines)


def inv_mix_column():
    lines = []
    lines.append("// InvMixColumns: 每列在 GF(2^8) 上乘逆矩阵, 系数 09 / 0b / 0d / 0e 用 xtime 组合出来.")
    lines.append("fn inv_mix_column(w: u32) -> u32 {")
    lines.append("    let a0 = (w >> 24u) & 0xffu;")
    lines.append("    let a1 = (w >> 16u) & 0xffu;")
    lines.append("    let a2 = (w >> 8u) & 0xffu;")
    lines.append("    let a3 = w & 0xffu;")
    for i in range(4):
        lines.append(f"    let b{i}2 = xtime(a{i});")
        lines.append(f"    let b{i}4 = xtime(b{i}2);")
        lines.append(f"    let b{i}8 = xtime(b{i}4);")
        lines.append(f"    let m{i}9 = b{i}8 ^ a{i};")
        lines.append(f"    let m{i}b = b{i}8 ^ b{i}2 ^ a{i};")
        lines.append(f"    let m{i}d = b{i}8 ^ b{i}4 ^ a{i};")
        lines.append(f"    let m{i}e = b{i}8 ^ b{i}4 ^ b{i}2;")
    # b0 = 0e*a0 ^ 0b*a1 ^ 0d*a2 ^ 09*a3
    plan = [
        ("0", [("e", 0), ("b", 1), ("d", 2), ("9", 3)]),
        ("1", [("9", 0), ("e", 1), ("b", 2), ("d", 3)]),
        ("2", [("d", 0), ("9", 1), ("e", 2), ("b", 3)]),
        ("3", [("b", 0), ("d", 1), ("9", 2), ("e", 3)]),
    ]
    for out, terms in plan:
        parts = [f"m{src}{coef}" for coef, src in terms]
        lines.append(f"    let r{out} = " + " ^ ".join(parts) + ";")
    lines.append("    return (r0 << 24u) | (r1 << 16u) | (r2 << 8u) | r3;")
    lines.append("}")
    return "\n".join(lines)


def aes_decrypt():
    lines = []
    lines.append("// 解密单个 16 字节块 (InvCipher), 输入密文块与密钥字, 返回 InvCipher 的结果.")
    lines.append("// 调用方再与密钥异或得到 CBC 首块明文.")
    lines.append("fn aes_inv_cipher(ct: array<u32, 4>, k0: u32, k1: u32, k2: u32, k3: u32) -> array<u32, 4> {")
    lines.append("    let rk = aes_expand(k0, k1, k2, k3);")
    lines.append("    var s0 = ct[0] ^ rk[40];")
    lines.append("    var s1 = ct[1] ^ rk[41];")
    lines.append("    var s2 = ct[2] ^ rk[42];")
    lines.append("    var s3 = ct[3] ^ rk[43];")
    lines.append("")
    for rnd in range(9, 0, -1):
        lines.append(f"    // 第 {rnd} 轮")
        lines.append("    {")
        lines.append("        let t = inv_shift_rows(array<u32, 4>(s0, s1, s2, s3));")
        lines.append("        s0 = inv_sub_word(t[0]);")
        lines.append("        s1 = inv_sub_word(t[1]);")
        lines.append("        s2 = inv_sub_word(t[2]);")
        lines.append("        s3 = inv_sub_word(t[3]);")
        lines.append(f"        s0 = s0 ^ rk[{4 * rnd}];")
        lines.append(f"        s1 = s1 ^ rk[{4 * rnd + 1}];")
        lines.append(f"        s2 = s2 ^ rk[{4 * rnd + 2}];")
        lines.append(f"        s3 = s3 ^ rk[{4 * rnd + 3}];")
        lines.append("        s0 = inv_mix_column(s0);")
        lines.append("        s1 = inv_mix_column(s1);")
        lines.append("        s2 = inv_mix_column(s2);")
        lines.append("        s3 = inv_mix_column(s3);")
        lines.append("    }")
        lines.append("")
    lines.append("    // 最后一轮不再做 InvMixColumns")
    lines.append("    {")
    lines.append("        let t = inv_shift_rows(array<u32, 4>(s0, s1, s2, s3));")
    lines.append("        s0 = inv_sub_word(t[0]) ^ rk[0];")
    lines.append("        s1 = inv_sub_word(t[1]) ^ rk[1];")
    lines.append("        s2 = inv_sub_word(t[2]) ^ rk[2];")
    lines.append("        s3 = inv_sub_word(t[3]) ^ rk[3];")
    lines.append("    }")
    lines.append("    return array<u32, 4>(s0, s1, s2, s3);")
    lines.append("}")
    return "\n".join(lines)


BODY = r"""
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
"""


def main():
    table_comment, box, inv = sbox_table()
    parts = [
        HEADER,
        "",
        table_comment,
        "",
        "// ---- MD5 ----",
        "",
        md5_block(),
        "",
        "// ---- AES-128 ----",
        "",
        inv_shift_rows(),
        "",
        inv_mix_column(),
        "",
        aes_key_schedule(),
        "",
        aes_decrypt(),
        BODY,
    ]
    print("\n".join(parts))


if __name__ == "__main__":
    main()
