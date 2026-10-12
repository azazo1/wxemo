# 使用说明

## 配置与账号发现

预编译 CLI 不需要 Python. 从源码开发时可用 `just run <参数>` 运行相同子命令.

```shell
wxemo config init --data-root <微信数据根目录>
wxemo accounts
wxemo doctor
```

数据目录的典型布局为 `xwechat_files/<账号>/business/emoticon`.
`--data-root` 可以指向 `xwechat_files` 或其上层, 程序向下查找最多 5 层.
账号目录 `wxid_example123456_7e1e` 对应账号标识 `wxid_example123456`.
多个账号时, 在 `seed` 和 `extract` 中使用 `--account` 明确选择.

配置目录默认是 `~/.config/wxemo/`, `XDG_CONFIG_HOME` 和 `WXEMO_CONFIG_DIR` 可以覆盖.
常规配置使用 `config.toml`, 账号密钥使用 `auth.json`, 两者带格式版本字段.
密钥文件不能公开分享. `config status` 会显示密钥内容, 不要将输出贴到公开页面.

## 搜索 seed

```shell
wxemo seed --account wxid_example123456
```

默认从持久化目录选取 3 个样本, 穷举 `0..4294967296`.
候选必须让所有样本首块命中已知格式, 命中后检查整文件填充并保存密钥.
可用 `--start`, `--end`, `--threads`, `--samples` 控制搜索范围和并行度.
非交互运行时建议加全局参数 `--no-interactive`.

搜索可能持续数分钟, 进度条和阶段日志用于观察是否仍在推进.
seed 的来源在二进制接口中是 64 位整数, `0..2^32` 是默认搜索范围,
不是对所有微信版本取值范围的保证.

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

## wxgf

解密后的 wxgf 是微信私有容器, 可能包含 HEVC 码流.
本工具尚未实现该容器的播放或转码, 也未确定其长度边界,
因此从 wxgf 头部开始保留剩余明文, 不按内部魔数猜测拆分.

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
