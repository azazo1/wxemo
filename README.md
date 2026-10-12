# wxemo

微信 4.x 自定义表情包的账号发现, seed 搜索与无损解密工具.
基于 Windows 微信 4.1.15.50 的静态分析, 处理本地表情数据, 不修改微信原始文件.

## 快速使用

从 [GitHub Release](https://github.com/azazo1/wxemo/releases) 下载对应平台的归档,
解压后将 `wxemo` 放到 PATH. 支持 Windows, Linux 和 macOS 的 x86_64 / aarch64.
Linux 归档使用 GNU 动态链接, 需要 glibc 2.39 或更新版本.

以下示例对应当前源码, 历史 Release 的命令请以 `--help` 为准.

```shell
wxemo accounts
wxemo seed search
wxemo seed search --backend metal
wxemo extract --out out/emoticons --manifest
wxemo decode out/emoticons --out out/images
```

默认目录会自动发现. 使用自定义位置时, 运行
`wxemo config init --data-root <微信数据根目录>` 保存路径.

多个账号时使用 `--account <账号标识或目录名>`. 根目录可以位于 `xwechat_files` 上层,
程序会向下查找账号. 在 WSL 中会把 `D:/wechat` 转为 `/mnt/d/wechat`.

- 配置保存在 `~/.config/wxemo/`, 可用 `WXEMO_CONFIG_DIR` 指定隔离目录.
- `seed search` 搜索并保存, `seed status` 查看状态, `seed import` 导入已知值或备份,
  `seed export` 导出敏感备份文件, `seed remove` 删除记录. `extract` 自动复用保存的密钥.
- `seed search --backend <auto|cpu|simd>` 选择搜索后端. 默认 `auto`:
  x86_64 有 AVX2 或 aarch64 有 NEON 时用多路 SIMD 的 MD5, 否则用标量多核.
  显式指定 `simd` 而本机不支持时直接报错, 不会静默退回标量.
- PNG / GIF / JPEG / WebP 按格式结构拆分. 未识别片段保留为 `.bin`.
- `.wxgf` 明文原样保留. `decode` 使用本机已有的微信媒体库导出完整 PNG/APNG,
  保留动画时序和透明度. 原生库不可用时明确报错, 平台条件见使用说明.
  使用 `--no-split` 可保留整个明文容器.
- `wxemo doctor` 检查配置与账号发现情况, `--verbose` 显示诊断日志.

更多用法见 [使用说明](docs/reproduction.md), 数据格式见 [格式说明](docs/format-notes.md).

## 从源码运行

需要 Rust 1.88 或更新版本, just 和 uv. `just dist` 使用 Python 标准库打包,
解释器由 uv 统一解析, 不依赖系统 tar / zip 或平台专用脚本.

```shell
just build
just test
just lint
just run --help
just dist
```

源码按命令层 `src/commands/` 与表情核心 `src/emoticon/` 组织.
seed 搜索的后端实现在 `src/emoticon/search/`: `scalar.rs` 是标量多核路径,
`simd/` 下按架构分成 `x86.rs` (AVX2, 8 路) 与 `aarch64.rs` (NEON, 4 路).
加密方案与反汇编证据见 [分析文档](docs/crypto-analysis.md).
CUDA 后端是可选编译的, 见下节.

`.cargo/config.toml` 为 aarch64 打开 `aes` crate 的 ARMv8 密码学扩展后端.
该 crate 在 x86_64 上默认会做运行期 AES-NI 检测, 在 aarch64 上却需要显式开启,
不开启就会静默使用纯软件实现, 使 Apple Silicon 等平台明显偏慢.
开启后仍由 `cpufeatures` 做运行期检测, 缺少该扩展的 CPU 照旧走软件路径.

## 加密方案

```text
key       = MD5(f"{seed}{wxid}EMOTICON").digest()
plaintext = AES-128-CBC(key, iv=key).decrypt(ciphertext)  # 去除合法 PKCS#7 填充
```

账号标识不带目录末尾的 4 位十六进制后缀. 默认搜索范围为 `0..2^32`,
可通过 `seed search --start / --end / --threads / --backend` 调整.
搜索速度随 CPU, 样本与后端变化.

候选消息不超过 55 字节, 填充后正好是一个 MD5 块, 因此 `simd` 后端可以一次算多路:
x86_64 用 AVX2 一次算 8 路, aarch64 用 NEON 一次算 4 路. AES 仍按路单独解密首块,
第一个样本不匹配的路立即淘汰. 两种后端的判定条件完全一致, 命中结果也一致.

## 搜索后端

`seed search` 默认是 `--backend auto`: 有可用的 NVIDIA 设备就走 CUDA,
否则回落 CPU 并记一条 `info` 日志. 也可以显式写 `--backend cpu` 或 `--backend cuda`,
显式指定 `cuda` 而环境不可用时直接报错, 不会静默改用 CPU.

CUDA 后端把 MD5 和 AES-128 都实现在 GPU 内核里, 一个线程试一个候选 seed,
命中后再回到 CPU 用同一套判定逻辑复核. 它是可选编译的:

```shell
just build-cuda
just test-cuda
```

默认的 `cargo build` 不带 `cuda` feature, 完全不受影响, macOS 上不需要 CUDA 工具链.
构建方式, 支持的环境和实测数据见 [使用说明](docs/reproduction.md#cuda-后端).

## 计算后端

`seed search --backend <auto|cpu|metal>` 选择搜索用的硬件, 默认 `auto`:
有可用的 Metal 设备就用 Metal, 否则回落 CPU, 并在日志里写明实际用的设备和后端.
显式指定 `metal` 而设备不可用时直接报错, 不静默回落.

Metal 后端是 macOS 上的原生 compute 实现: 每个 GPU 线程处理一个候选 seed,
在 kernel 里完成十进制拼串, 单块 MD5, AES-128 密钥扩展与首块解密.
MSL 源码内嵌在二进制里, 在运行期由驱动自带的编译器编译,
不需要安装 Xcode 的 Metal Toolchain 组件.
两个后端的命中判定条件与命中后的整文件复核完全共用, 只可能在速度上有差别.

## 免责声明

仅用于学习和自查自己的本地微信数据. 不联网上传数据, 不修改微信数据库.
seed 与 key 是账号级密钥材料, 不要分享密钥文件或包含它们的命令输出.
本项目不保证适用于其他微信版本, 请遵守当地法律法规.
