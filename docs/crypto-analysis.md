# 加密方案的证据链

分析对象: Windows 微信 4.1.15.50 的 `Weixin.dll` (204 MB, x86-64 PE, 镜像基址 `0x180000000`).

以下地址均为 VA (镜像基址 + RVA). 结论:

```
key       = MD5(f"{seed}{wxid}EMOTICON").digest()[:16]
plaintext = AES-128-CBC(key, iv=key).decrypt(ciphertext)
```

## 1. 错误文案指出"取密钥"是独立环节

`.rdata` 中有一组紧邻的错误文案:

```
emoticon file invalid
emoticon file read error
emoticon data decrypt key error
emoticon data decrypt failed
emoticon md5 verify failed
emoticon wxam data empty and no retry
emoticon data encrypt key failed
emoticon data save error
```

`decrypt key error` 与 `decrypt failed` 分开出现, 说明"取密钥"和"解密数据"是两个
独立的失败点, 且取密钥本身会失败. 这组文案全部由同一个函数 `0x180931070` 引用,
该函数即解密主流程.

## 2. 密钥派生函数

函数 `0x180eead20` 是派生逻辑:

1. `0x180eead48` 取全局单例 (`0x180045d10` 返回 `0x18bb61c90` 处的指针),
   `[vtable+0x68]` 判断是否就绪;
2. `0x180eead85` 调 `[vtable+0x18]`, 得到一个 64 位整数, 即账号级 `seed`;
3. `0x180eeadc0`-`0x180eeadeb` 用乘 `0xcccccccccccccccd` 再右移的除法序列做十进制
   转换, 把该整数逐位写入栈上缓冲区;
4. `0x180eeaebc`-`0x180eeaefe` 追加 `[vtable+0x20]` 返回的账号标识字符串;
5. `0x180eeafcd`-`0x180eeafd6` 追加字面量 `"EMOTICON"`. 源码里以 `movabs` 立即数
   `0x4e4f4349544f4d45` 内联 (小端即 `EMOTICON`), 长度 8;
6. `0x180eeb046` 调 `0x1805abc60` 做哈希;
7. `0x180eeb053` 调 `0x18022bea0` 把哈希结果还原成字节.

拼接顺序由上面第 3, 4, 5 步的调用次序确定: seed 在前, 账号标识居中, 后缀在最后.

## 3. 哈希是 MD5

`0x1805abc60` 调用 `0x187533ec0`, 后者内部在 `0x187533bb0` 初始化 IV 为
`0x67452301 / 0xefcdab89 / 0x98badcfe / 0x10325476`, 这是标准 MD5 的初始常量.

`0x1805abc60` 返回的不是原始摘要, 而是它的十六进制串: 先调 `0x1805abb80`, 后者以
`mov r8d, 0x10` 调 `0x18058f290`, 用字符表 `"0123456789ABCDEF"` (rva `0x8f3df90`)
把 16 字节摘要编码成 32 个字符, 再调 `0x180307860` 转小写.

随后 `0x18022bea0` 把该十六进制串解码回字节: 每两个字符合成一字节
(`shl dl, 4; or dl, r8b`), `0-9` 映射为 `0-9` (`sub ecx, 0x30`),
`a-f` 与 `A-F` 映射为 `10-15` (`add cl, 0xa9` / `add cl, 0xc9`).

一编码一解码相抵消, 所以 `0x180eead20` 的最终产物就是 **MD5 摘要的原始 16 字节**.

## 4. 账号标识不带目录后缀

`0x180eeac60` 是取密钥的包装函数, 它调 `0x180047220` 取得账号标识, 再交给
`0x180385af0` 组装密钥. 该标识来自账号管理单例, 是登录后的 wxid,
与磁盘上的账号目录名无关 —— 目录名尾部会多出一段 4 位十六进制后缀
(例如 `wxid_example123456_7e1e`), 那段后缀**不参与**派生.

实测印证: 用不带后缀的 wxid 搜索立即命中, 用带后缀的形式则在整个 2^32 空间内无解.

## 5. 密钥长度 16 字节

`0x180385af0` 从对象 `+0x2d8` 取出密钥字符串后, 对 32 字节常量做循环异或
(内存顺序 `695dfc07e34228d8302b4866113868ea666c0ab51c168c1e56f6500c113d8374`).

`0x1805abb80` 把密钥规整为固定长度: 以 `mov r8d, 0x10` 构造长度 16 的缓冲区,
与 AES-128 的密钥长度一致.

## 6. 分组模式是 CBC

- 二进制内含 `AES for Intel AES-NI, CRYPTOGAMS by <appro@openssl.org>` 与
  `aesni_init_key` / `aes_init_key` 等标识, 说明内置 OpenSSL 的 AES 实现.
- 二进制内含 `aes-128-cbc` 与 `AES-128-CBC` 字面量, 指明分组模式为 CBC.
- `0x180fd55b0` 是密码引擎初始化, 入口校验密钥长度 `& 0xf == 0` 且 `>= 0x10`,
  符合 AES 密钥编排特征.
- 解密器构造在 `0x1846de7c0` / `0x1846d3b50`, 采用 `std::codecvt` 风格的回调对象,
  与 OpenSSL `EVP_CipherInit` 的用法一致.

IV 等于密钥: 这是实测确认的 —— 用 `iv = key` 解密后, 全部 616 个样本文件的
PKCS#7 填充都合法, 且首块命中已知图片魔数, 不存在第二个自洽的解释.

## 7. 填充

解密结果的末尾符合 PKCS#7: 填充字节的值等于填充长度, 取值 1 到 16.
616 个样本全部满足, 无例外.

## 8. 密文位置

模块内与表情包存储相关的路径字面量:

```
PersistenceStore
Config
Storage
thumb
EMOTICON
Emoticon
_thumb
pages/emoticons/
```

对应磁盘目录 `xwechat_files/<账号>/business/emoticon/{Persist,PersistStore,Thumb,ThumbStore,Temp}`.

## 9. 容器格式

`Persist` 与 `Thumb` 下的文件基本是一张图一密文; `PersistStore` 与 `ThumbStore`
下的文件可能是多张图片首尾相接的长容器. 例如某个 263678 字节的文件里,
按 PNG 魔数与 `IEND` 切分可得 24 段, 每段都是完整且 CRC 正确的 PNG.

切分逻辑见 `scripts/split_container.py`: 按各格式的起始魔数与结束标记配对切分.

## 交叉验证

公开项目 [stilburn007/wechat-emoticon-exporter](https://github.com/stilburn007/wechat-emoticon-exporter)
给出同样的公式与分组模式. 本次分析在以下几点上独立印证了该实现:

1. 后缀字面量确为 `EMOTICON`, 以 8 字节立即数内联;
2. 拼接顺序为 seed 在前, 账号标识居中, 后缀最后;
3. 参与加密的密钥长度为 16 字节;
4. 账号标识取不带目录后缀的形式;
5. IV 等于密钥, 且填充为 PKCS#7.

该实现同时尝试"带后缀"与"不带后缀"两种写法, 说明它没有从二进制确定这一点;
本次分析把它定死了.
