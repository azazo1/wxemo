# 从零复现

目标: 从一台装了微信 4.x 的机器上, 把自定义表情包还原成可以直接查看的图片.

## 前置条件

- Windows 微信 4.x 已登录过, 账号目录 `xwechat_files/<账号>/business/emoticon` 存在.
- Rust 工具链 (stable) 与 Python 3.10 以上.
- Python 侧需要 `pycryptodome`:

  ```shell
  pip install pycryptodome
  ```

## 第一步: 确认账号标识

看账号目录名:

```
xwechat_files/wxid_example123456_7e1e/
```

去掉尾部的 `_7e1e` 之后, 剩下的 `wxid_example123456` 就是派生用的账号标识.
**不要**把 `_7e1e` 带进去.

## 第二步: 求出 seed

`seed` 是 32 位无符号整数, 由微信在运行期生成, 只存在于 `Weixin.exe` 进程内存里,
磁盘上没有. 因此用穷举法求:

```shell
cd seedsearch
cargo build --release

./target/release/seedsearch \
  --sample ../emoticon/Persist/aa/aab4337d6f48f3a15a830f7c0c98042b \
  --sample ../emoticon/Persist/a1/a1171de0bff921ddab5bc384ed6593a5 \
  --sample ../emoticon/Persist/de/dee2c5b317b91a19807af35f49b9119f \
  --wxid wxid_example123456 \
  --start 0 --end 4294967296 \
  --out seed-result.txt
```

要点:

- 样本至少要 3 个, 且取自不同的表情. 判定要求全部样本的首块都命中图片魔数,
  样本越多误报越少. 3 个样本时误报期望已经低于 1e-5.
- 样本文件需要是 16 字节的正整数倍 (微信的密文都是).
- 范围 `0..4294967296` 覆盖全部 32 位取值. 16 核机器上大约 1.5 分钟.

命中后程序打印并写入 `seed-result.txt`:

```
seed  = <十进制整数>
wxid  = wxid_xxxxxxxx
key   = <32 位十六进制>
格式  = png
```

## 第三步: 还原全部表情包

```shell
python3 scripts/extract_emoticons.py <密文目录> out/emoticons --key <上一步的 key>
```

脚本做三件事:

1. 用 `AES-128-CBC(key, iv=key)` 解密每个文件, 去掉 PKCS#7 填充;
2. 按魔数把多图拼接的容器切分;
3. 校验每个切片 (PNG 会逐 chunk 校验 CRC), 按原目录层次写出.

输出示例:

```
解密文件 616 个, 写出图片 921 张
格式分布: {'png': 346, 'wxgf': 265, 'jpg': 186, 'gif': 123, 'webp': 1}
填充异常 0 个, PNG 结构异常 0 个
```

`填充异常` 与 `PNG 结构异常` 都应为 0. 如果不是 0, 说明密钥不对或程序有 bug.

## 关于 wxgf 格式

`.wxgf` 是微信自己的动图容器, 内部是 H.265/HEVC 码流. 它本身不加密,
但需要转码才能当普通图片看:

```shell
ffmpeg -i input.wxgf -vf "split[s0][s1];[s0]palettegen[p];[s1][p]paletteuse" -loop 0 output.gif
```

如果直接喂给 ffmpeg 失败, 尝试跳过文件头到第一个 VPS/SPS NAL 单元:

```python
markers = [pos for pos in (
    data.find(b"\x00\x00\x00\x01\x40\x01"),  # VPS
    data.find(b"\x00\x00\x00\x01\x42\x01"),  # SPS
) if pos >= 0]
start = min(markers) if markers else 0
open("payload.h265", "wb").write(data[start:])
```

## 常见问题

**搜完整个范围没有命中.** 依次检查:

1. 账号标识是否带了目录后缀. 这是最常见的原因.
2. 样本文件是否取自加密目录 (`Persist` / `PersistStore`), 而不是明文缓存.
3. 微信版本是否与分析方法一致. 公式在大版本之间可能变化.

**`seed` 一定在 32 位范围内吗.** 二进制里 `seed` 来自 `[vtable+0x18]` 返回的
64 位整数, 但实际取值远小于 2^32. 如果确实没命中, 可以扩大到 2^40 再试,
代价是时间乘以 256.

**能不能直接从进程内存读 seed.** 可以, 而且比穷举快. 做法是扫描
`Weixin.exe` 的内存找 8-10 位十进制整数候选, 逐个代入公式验证. 本项目的
穷举路径不需要 Windows 环境, 在 Linux 上也能跑, 所以把它作为主路径.

## 自验证

改动搜索器之后, 跑一遍端到端自验证:

```shell
python3 scripts/selfcheck.py seedsearch/target/release/seedsearch .tmp/selfcheck
```

它会用一个已知 seed 造出 PNG / GIF / JPEG 三种格式的密文样本,
再让搜索器在小区间内把它们找回来. 这是为了确认 CBC 的 IV 异或、
魔数判定、消息拼接这几处没有写错.
