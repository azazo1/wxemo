# 使用说明

## 配置与账号发现

预编译 CLI 不需要 Python. 从源码开发时可用 `just run <参数>` 运行相同子命令.

```shell
wxemo accounts
wxemo config init
wxemo doctor
```

未指定路径且未配置数据根目录时, 自动探测系统默认候选:

| 平台 | 候选位置 |
| --- | --- |
| macOS | `~/Library/Containers/com.tencent.xinWeChat/Data/Documents/xwechat_files` |
| Windows | 系统 Documents 位置下的 `xwechat_files`, 同时考虑 OneDrive 和 Tencent 嵌套目录 |
| Linux | XDG 文档目录或 `~/Documents/xwechat_files`, `~/xwechat_files`, XDG 的 wechat 沙箱目录 |

Windows 使用 Known Folder API 支持 Documents 重定向.
Linux 优先读 `XDG_DOCUMENTS_DIR`, 再解析 `user-dirs.dirs`, 不执行其中的 shell 内容.
只采用包含 `<账号>/business/emoticon` 的有效位置, 不扫描整个磁盘.
`accounts`, `seed search`, `extract`, `doctor` 可在未初始化配置时使用发现结果.
`config init` 找到一个有效位置会自动保存, 多个位置时需选择或用 `--data-root` 指定.
多账号仍需交互选择或显式指定 `--account`.

优先级为显式 `--data-root` > 已保存配置 > 系统默认候选.
指定过自定义路径时不会被默认位置覆盖. 找不到时使用
`wxemo config init --data-root <目录>` 手动设置.
旧版微信的目录布局和 macOS / Linux 的实际加密兼容性尚未用真实样本验证,
目录发现成功不代表解密一定成功.

候选依据: [腾讯 Lemon 的 macOS 路径](https://github.com/Tencent/lemon-cleaner/blob/7ff8676f1dbe624b039f6b5d50ca81b50963629e/localPod/LemonFileMove/LemonFileMove/Classes/Scan/LMWeChatScan.m),
[Windows 路径参考](https://github.com/sgaofen/murmur/blob/31646a003a0921217f90f947e2bc33f0de7bdbed/docs/ONBOARDING_WINDOWS.md),
[Linux 原生存储示例](https://github.com/RICwang/docker-wechat/blob/49cc62e441dba7ad07dd6f7aa000b1ae35a73ffe/README.md)
与 [Gentoo 沙箱布局](https://github.com/gentoo-zh/overlay/blob/82b3fce9745dc1095b0585cd29d2820a1d67ba99/net-im/wechat/files/bwrap.sh).

数据目录的典型布局为 `xwechat_files/<账号>/business/emoticon`.
`--data-root` 可以指向 `xwechat_files` 或其上层, 程序向下查找最多 5 层.
账号目录 `wxid_example123456_7e1e` 对应账号标识 `wxid_example123456`.
多个账号时, 在 `seed` 和 `extract` 中使用 `--account` 明确选择.

配置目录默认是 `~/.config/wxemo/`, `XDG_CONFIG_HOME` 和 `WXEMO_CONFIG_DIR` 可以覆盖.
常规配置使用 `config.toml`, 账号密钥使用 `auth.json`, 两者带格式版本字段.
密钥文件不能公开分享. `config status` 会显示密钥内容, 不要将输出贴到公开页面.

## macOS 容器访问授权

探测微信容器时, 程序会主动尝试读取目录, 而不是只检查路径是否存在.
macOS 14+ 可能因此显示其他应用容器的访问授权提示, 请按需要允许或拒绝.
系统决定是否弹窗, 已有拒绝记录或终端宿主的权限可能影响结果.
程序不会重置 TCC 决定, 也不能自行获取完全磁盘访问权限.

访问被拒绝时会报告具体路径并保留系统错误, 不再当作目录不存在继续要求输入路径.
授权后仍找不到账号时, 才继续检查是否具有 `<账号>/business/emoticon` 布局.
当前实现不包含文件夹选择器或持久安全书签, 保存路径不等于保存系统授权.
macOS 专属处理位于 `src/platform/macos/`.
依据见 [Apple 容器访问文档](https://developer.apple.com/documentation/security/accessing-files-from-the-macos-app-sandbox).

## 搜索 seed

```shell
wxemo seed search --account wxid_example123456
```

默认从持久化目录选取 3 个样本, 穷举 `0..4294967296`.
候选必须让所有样本首块命中已知格式, 命中后检查整文件填充并保存密钥.
可用 `--start`, `--end`, `--threads`, `--samples` 控制搜索范围和并行度.
非交互运行时建议加全局参数 `--no-interactive`.

搜索可能持续数分钟, 进度条和阶段日志用于观察是否仍在推进.
seed 的来源在二进制接口中是 64 位整数, `0..2^32` 是默认搜索范围,
不是对所有微信版本取值范围的保证.

### 计算后端

`--backend <auto|cpu|metal>` 选择用哪种硬件搜索, 默认 `auto`.

| 取值 | 行为 |
| --- | --- |
| `auto` | macOS 上能用 Metal 就用 Metal, 否则回落 CPU. 日志会写明实际选了哪个和哪个设备 |
| `cpu` | 只用 CPU 的 rayon 多核实现, 所有平台可用 |
| `metal` | 只用 macOS 的 Metal compute, 设备不可用时明确报错, 不静默回落 |

```shell
wxemo seed search --account wxid_example123456 --backend metal
wxemo seed search --account wxid_example123456 --backend cpu
```

两个后端的命中判定和命中后的整文件复核是同一段代码, 结果只可能在速度上有差别.
Metal 后端的 MSL 源码在运行期由驱动自带的编译器编译, 不需要安装构建期的
metal 编译器 (`MetalToolchain` 组件缺少时也能用).
首次提交前有约 0.3 秒的运行期编译开销, 搜索范围越大这部分占比越低.
`--threads` 只影响 CPU 后端, Metal 后端的并行度由 GPU 决定.
非 macOS 平台上 `--backend metal` 会直接报错, `auto` 会说明回落原因.

## 管理 seed 记录

```shell
wxemo seed status
wxemo seed status --account wxid_example123456
wxemo seed import <已知十进制seed> --account wxid_example123456
wxemo seed remove --account wxid_example123456
```

- `status` 仅列出保存状态, 不显示 seed 或密钥. 不依赖微信数据目录存在.
- `import` 从已知 seed 派生并保存密钥, 不搜索也不验证密文.
  已有记录时必须加 `--force` 才能覆盖. 导入后使用 `extract` 核对解密结果.
- `remove` 只删除本工具保存的该账号记录, 不修改微信数据.
  交互运行会确认, 非交互运行必须显式加 `--yes`.
- `search` 是原 `wxemo seed` 的替代入口. 直接运行 `wxemo seed` 只显示用法,
  不会启动长时间搜索.

## 备份与恢复 seed

```shell
wxemo seed export --out secrets/seed-backup.json
wxemo seed export --account wxid_example123456 --out secrets/account-seed.json
wxemo seed import --file secrets/seed-backup.json
wxemo seed import --file secrets/seed-backup.json --account wxid_example123456
```

- `export` 要求显式指定 `--out`, 未指定账号时导出全部已保存记录.
- 备份是带 `version` 的 JSON, 只保存账号与 seed. 恢复时重新派生 key.
  文件是明文且足以派生解密密钥, 不要公开分享或提交到 Git.
- 默认不覆盖输出文件, 需要时显式加 `--force`.
  即使加了 `--force`, 也不能覆盖正在使用的配置或密钥文件, 不替换符号链接或目录.
- Unix 下导出文件在创建时即使用 `0600` 权限. Windows 使用目标目录的访问控制,
  请把文件放在仅自己可访问的位置.
- 导入文件前检查格式版本, 账号和已有记录冲突. 校验失败不写入任何导入记录.
  已有记录需要 `--force` 才会覆盖, 不删除备份中未包含的其他账号.
- 单个数值导入仍使用 `seed import <seed> --account <账号>`, 不能同时提供 seed 和 `--file`.
- 导出和文件恢复的日志只显示数量或路径, 不打印 seed 或 key.
  导入只派生密钥, 不代表已验证当前微信数据的兼容性.

## 解密与输出

```shell
wxemo extract --account wxid_example123456 --out out/emoticons --manifest
```

也可用 `--key <32位十六进制>` 或 `--seed <整数>` 指定密钥来源.
`--emoticon-dir <目录>` 跳过账号发现, 目录应保持 `<账号>/business/emoticon` 布局.

1. 按 AES-128-CBC 解密, IV 等于密钥.
2. 只移除合法 PKCS#7 填充, 填充异常时保留解密结果并警告.
3. 按 PNG / GIF / JPEG / WebP 的块结构顺序切分, 不扫描已识别片段的内部.
4. 未识别的前缀, 间隙和尾注写为 `.bin`, wxgf 写为 `.wxgf`.

输出保留原目录结构. 单片段文件追加格式扩展名,
多片段追加 `.000`, `.001` 等顺序编号后再追加扩展名.
按编号拼接同一个源文件的输出可恢复完整的去填充明文.
`--no-split` 直接输出完整明文. `--manifest` 额外生成片段路径, 格式和大小清单.

格式解析只负责确定边界, 不等价于图像解码或 PNG CRC 校验.
总字节数校验可以发现数据遗漏, 但不能独立证明密钥正确.

## WXGF 图片与动画解码

`extract` 仍保留完整 `.wxgf` 明文, 不改变原来的字节守恒行为.
使用独立的 `decode` 命令转成可打开的 PNG/APNG, 不读取账号配置或密钥:

```shell
wxemo decode image.wxgf --out out/images
wxemo decode out/emoticons --out out/images
wxemo decode out/emoticons --out out/images --decoder-lib <可信的原生媒体库路径>
```

从源码运行相同操作可用 `just run decode <输入> --out <输出目录>`.
单文件按文件头验证, 目录输入递归处理 `.wxgf` (忽略后缀大小写), 不跟随目录内的符号链接.
输出保留子目录结构, 后缀替换为 `.png`. 多帧文件内部为 APNG, 使用支持 APNG 的浏览器
或图片查看器打开. 输出目录不能等于输入目录或位于其内部, 已有图片不覆盖.

### 原生解码库

完整动画不能只拼接 HEVC 码流: WXGF 还保存帧时序, 蒙版与原始画布尺寸.
`decode` 使用用户本机已有的版本化 `wxam_dec_wxam2pic_5` 接口, `mode=2` 输出 PNG/APNG,
保留全部动画帧, 循环信息, 逐帧时长和 RGBA 透明度, 不转换成只有二值透明度的 GIF.

- macOS 自动查找系统或用户 Applications 下已有微信的 `MultiMediaDyn.framework`.
- Windows 自动查找常见 Tencent 安装目录及版本子目录中的 `VoipEngine.dll`.
  非默认位置用 `--decoder-lib` 指定, 依赖 DLL 需与媒体库同目录或位于系统安全搜索目录.
- Linux 不承诺存在可用媒体库, 需要显式提供当前系统和架构兼容的原生库.
  Windows DLL 不能直接在 Linux 加载. 找不到兼容库时明确报错, 不静默退化成动画首帧.
- 库的 CPU 架构必须与 wxemo 一致. 本项目不下载, 复制或分发微信媒体库, 也不需要 FFmpeg.
  仅加载可信的本机微信库, 不使用来历不明的动态库.

每个文件在独立工作进程中解码, 私有库崩溃或超时不会终止其他文件的处理.
默认单文件超时为 120 秒, 可用 `--timeout-secs` 调整. 任一文件失败时最终返回非零退出码,
已成功的图片保留. 输入限制 64 MiB, 编码输出限制 128 MiB, 总帧像素数限制 256 Mi 个,
超过限制明确报错. 支持已验证的 WXGF 布局版本 0 和 2, 未知版本不猜测处理.

写出前校验 PNG chunk CRC, APNG 序号, 尺寸和帧数与 WXGF 头部一致,
剔除原生库在 IEND 后追加的私有尾注. 校验不通过的文件不发布图片.
这些检查用于防止截断或首帧输出被当作成功, 不是对所有微信版本的兼容性保证.
原生接口参考 [公开 WXAM 调用示例](https://github.com/HShiDianLu/wx-dat2img/blob/main/main.py),
完整 APNG 行为已在 macOS arm64 上用真实静态, 动画与透明样本验证; Windows / Linux
仍需在相应运行环境验证, 不能仅凭接口名相同认定行为一致.

## 排查问题

搜索没有命中时, 检查账号选择, 输入是否来自加密的持久化目录,
以及微信版本是否适用. 填充异常时核对账号, seed 与密钥, 不要直接删去末尾字节.

`extract` 输出目录应位于输入目录之外, 避免把已输出文件再次当作输入.
使用新的空输出目录可以避免上次运行留下的片段混入结果.

## 开发验证

```shell
just test
just lint
```

测试覆盖已知种子的搜索, CBC 解密, 截断输入保留,
容器边界, 嵌入魔数去重以及未知数据的逐字节重组.
WXGF 测试覆盖头部版本, PNG/APNG 完整帧数与 CRC, 空数据分块,
原生工作进程失败后的继续处理, 超时, 输出防覆盖和路径越界.
合成媒体库及透明 APNG 用于可移植测试, 不依赖 CI 安装微信或持有私人样本.

本机 macOS arm64 的额外验证处理了 263 个真实 WXGF 文件:
181 个静态图片, 82 个动画, 合计 3195 帧均可由独立图片解码器读取.
90 个 GIF 类容器 (含单帧) 的逐帧时长与源记录一致,
4 个半透明样本的 alpha 像素与原 HEVC 蒙版逐像素一致.
原文件 SHA-256 在转换前后不变. 此结果只说明该样本集和媒体库组合已验证,
不代表所有 WXGF 布局或其他平台的兼容性保证.
