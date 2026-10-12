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
- `seed search --backend <名字>` 选择搜索后端, 默认 `auto`. 详见下节.
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
加密方案与反汇编证据见 [分析文档](docs/crypto-analysis.md).

## 加密方案

```text
key       = MD5(f"{seed}{wxid}EMOTICON").digest()
plaintext = AES-128-CBC(key, iv=key).decrypt(ciphertext)  # 去除合法 PKCS#7 填充
```

账号标识不带目录末尾的 4 位十六进制后缀. 默认搜索范围为 `0..2^32`,
可通过 `seed search --start / --end / --threads / --backend` 调整.
搜索速度随 CPU, 样本与后端变化.

候选消息不超过 55 字节, 填充后正好是一个 MD5 块, 各个后端都利用这一点: SIMD 一次算
多路, GPU 内核一个线程处理一个候选, 都省略了跨块处理.

## 搜索后端

`seed search --backend <auto|cpu|simd|cuda|metal|wgpu>` 选择搜索用的硬件, 默认 `auto`.
`auto` 按本机能力挑选, 优先级为 CUDA > Metal > wgpu > SIMD > 标量,
并在日志里写明实际选了哪个. 显式指定某个后端而本机不支持时直接报错, 不会静默回落,
否则用户会以为在用加速路径而实际跑的是标量.

后端实现在 `src/emoticon/search/`:

- `scalar.rs` 标量多核路径, 用 rayon 分块, 是其他后端不可用时的兜底.
- `simd/` 多路向量化 MD5. `x86.rs` 用 AVX2 一次算 8 路, `aarch64.rs` 用 NEON 一次算 4 路.
- `cuda/` NVIDIA 后端, 需要 `cuda` feature, 见下节.
- `metal/` macOS 原生 compute, MSL 源码内嵌在二进制里, 运行期由驱动编译,
  不需要安装 Xcode 的 Metal Toolchain 组件.
- `gpu/` wgpu 跨平台后端, 一份 WGSL 覆盖 Metal / Vulkan / DX12.

GPU 后端只做首块魔数筛选, 命中后一律回到 CPU 用与标量路径完全相同的 `confirm`
复核, 因此各后端的判定条件与命中结果必然一致, 差别只可能在速度上.

### aarch64 的 AES 必须显式开启

`.cargo/config.toml` 为 aarch64 打开 `aes` crate 的 ARMv8 密码学扩展后端.
该 crate 在 x86_64 上默认会做运行期 AES-NI 检测, 在 aarch64 上却需要显式开启,
不开启就会静默使用纯软件实现, 使 Apple Silicon 等平台明显偏慢 (实测约 2.5 倍).
开启后仍由 `cpufeatures` 做运行期检测, 缺少该扩展的 CPU 照旧走软件路径.

### CUDA 后端需要显式编译

CUDA 后端默认不参与编译, macOS 与 Linux 上没有 CUDA 工具链也能正常 `cargo build`:

```shell
just build-cuda
just test-cuda
```

构建脚本会用 nvcc 把 `kernel.cu` 编成 cubin 再嵌进二进制, 运行期经 `libloading`
直接调用 CUDA driver API, 因此不引入任何新的 crate 依赖. WSL 里没有 nvcc 与
libnvrtc, 构建脚本会经 interop 调用 Windows 侧的 nvcc.

## 免责声明

仅用于学习和自查自己的本地微信数据. 不联网上传数据, 不修改微信数据库.
seed 与 key 是账号级密钥材料, 不要分享密钥文件或包含它们的命令输出.
本项目不保证适用于其他微信版本, 请遵守当地法律法规.
