# 格式说明

## 磁盘布局

账号目录下与表情包相关的部分:

```
business/emoticon/
├── Persist/          按前两位十六进制分桶, 一个文件一张图
├── PersistStore/     同上, 但一个文件可能是多张图拼接的长容器
├── Thumb/            缩略图, 文件名带 .thumb 后缀
├── ThumbStore/       缩略图的多图容器, 文件名带 .icon 后缀
└── Temp/             临时目录, 通常为空
```

文件名是内容的 MD5 十六进制串, 不带扩展名. 桶目录取文件名的前两个字符.

## 密文

每个文件都是 AES-128-CBC 密文:

- 密钥与 IV 都来自 `MD5(f"{seed}{wxid}EMOTICON").digest()[:16]`;
- 文件长度都是 16 的整数倍;
- 末尾是 PKCS#7 填充, 去掉后才是原始数据.

## 明文

去掉填充后的明文可能是:

| 格式 | 起始魔数 | 结束标记 |
| --- | --- | --- |
| PNG | `89 50 4E 47 0D 0A 1A 0A` | `IEND` chunk |
| GIF | `GIF87a` / `GIF89a` | `3B` |
| JPEG | `FF D8 FF` | `FF D9` |
| WebP | `RIFF` ... `WEBP` | 由 RIFF 长度字段决定 |
| BMP | `BM` | 由文件头长度字段决定 |
| wxgf | `wxgf` | 无明确标记 |

## 容器切分

`PersistStore` 与 `ThumbStore` 下的大文件由多张图片首尾相接而成.
切分规则是扫描起始魔数并与对应的结束标记配对:

- PNG: 从魔数找到 `IEND`, 再加上 4 字节 CRC 作为终点;
- GIF: 从 `GIF8` 找到第一个 `3B`;
- JPEG: 从 `FF D8 FF` 找到 `FF D9`.

切分之后每段都应当能独立解码. 校验方式: PNG 逐 chunk 核对长度与 CRC,
GIF / JPEG 用解码器试开.

实测一个 263678 字节的 `PersistStore` 文件可切出 24 段 PNG,
每段 CRC 都正确, 说明切分点选得对.

## wxgf

`.wxgf` 是微信自己的动图容器, 内部是 H.265/HEVC 码流, 不加密.

文件头之后是标准的 HEVC NAL 单元, 第一个通常是 VPS (`00 00 00 01 40 01`)
或 SPS (`00 00 00 01 42 01`). 转码方式见 `reproduction.md`.
