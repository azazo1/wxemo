# 微信 4.x 自定义表情包加密方案

对 Windows 微信 4.1.15 的 `Weixin.dll` 做静态分析, 还原自定义表情包的加解密方案,
并给出从密文还原出图片的完整流程.

## 结论

```
key       = MD5(f"{seed}{wxid}EMOTICON").digest()[:16]
plaintext = AES-128-CBC(key, iv=key).decrypt(ciphertext)   # PKCS#7 填充
```

- `wxid` 是账号标识, 取**不带**目录名尾部 `_xxxx` 后缀的形式.
  例如目录名 `wxid_example123456_7e1e` 对应 `wxid_example123456`.
- `seed` 是账号级随机种子, 只存在于运行中的 `Weixin.exe` 进程内存里, 磁盘上没有.
- 密文解密后可能是一张图片, 也可能是**多张图片首尾相接**的容器, 需要再按魔数切分.

## 用法

需要 Rust 工具链, 以及 Python 3.10 以上 (仅解密脚本需要).

```shell
# 1. 求出当前账号的 seed (暴力搜索, 16 核约 1.5 分钟)
just search <账号目录>/business/emoticon

# 2. 用求出的 key 还原全部表情包
just extract <账号目录>/business/emoticon out/emoticons <key>
```

也可以手动调用:

```shell
cd seedsearch && cargo build --release

./target/release/seedsearch \
  --sample <任意表情密文文件> --sample <另一个> --sample <第三个> \
  --wxid <账号标识> \
  --start 0 --end 4294967296 \
  --out seed-result.txt

python3 scripts/extract_emoticons.py <密文目录> <输出目录> --key <32 位十六进制>
```

## 目录

```
docs/crypto-analysis.md   加密方案的证据链 (反汇编地址与推理过程)
docs/reproduction.md      从零复现的完整步骤
docs/format-notes.md      密文文件与容器的格式说明
seedsearch/               多核 seed 暴力搜索器 (Rust)
scripts/                  解密, 容器切分, 自验证脚本 (Python)
```

## 为什么需要暴力搜索 seed

`seed` 由微信在运行期生成, 不落盘, 因此无法从磁盘直接读出. 它参与密钥派生,
但空间有限 (32 位无符号整数), 所以可以穷举:

对每个候选 seed 算出密钥, 解出样本文件的第一个分组, 看是否命中图片魔数.
这个判定很快, 因为每个候选只需要一次 MD5 和几次 AES 分组解密.

实测 16 核约 5000 万候选/秒, 搜完 2^32 约 1.5 分钟. 命中后会自动用整文件解密复核.

## 免责声明

本项目仅用于分析本地微信客户端自身的数据格式, 供学习和数据自查使用.

- 只读取本机微信数据, 不修改微信的任何文件或数据库.
- 不涉及网络通信, 不上传任何数据.
- `seed` 与派生出的 `key` 是账号级密钥材料, **不要公开分享**.
  本文档与代码中一律使用占位符, 不包含任何真实账号的值.

请遵守当地法律法规, 不要用它处理他人的数据.
