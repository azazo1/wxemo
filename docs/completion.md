# Shell 补全

`wxemo completion <shell>` 将补全脚本写入标准输出. 支持 `bash`, `zsh`, `fish`, `powershell` 和 `elvish`, 覆盖子命令, 全局参数, 搜索后端枚举以及文件和目录路径. 脚本由 clap_complete 从当前版本的 CLI 定义生成, 不查询微信账号或密钥.

补全脚本由 clap_complete 从当前 CLI 定义生成, 只是薄薄一层注册: 补全时调用 PATH 中的 `wxemo` 取候选, 因此需要先把可执行文件放入 PATH, 不要把它当静态列表缓存下来. 更新 wxemo 后按原命令重新生成并加载一次即可.

## Fish

```shell
mkdir -p ~/.config/fish/completions
wxemo completion fish > ~/.config/fish/completions/wxemo.fish
```

打开新的 Fish 会话后生效, 也可在当前会话运行 `source ~/.config/fish/completions/wxemo.fish`.

## Zsh

```shell
mkdir -p ~/.zfunc
wxemo completion zsh > ~/.zfunc/_wxemo
```

在 Zsh 启动文件中, 将补全目录放入 `fpath` 后再初始化补全:

```zsh
fpath=(~/.zfunc $fpath)
autoload -Uz compinit
compinit
```

已有 `compinit` 初始化时只需在它之前添加 `fpath` 设置, 然后打开新会话.

## Bash

安装了 bash-completion 时:

```shell
mkdir -p ~/.local/share/bash-completion/completions
wxemo completion bash > ~/.local/share/bash-completion/completions/wxemo
```

打开新会话后生效. 也可直接生成文件, 然后在 Bash 中用 `source` 加载, 这种方式不依赖 bash-completion.

## PowerShell

在 PowerShell 中运行, 当前会话即生效:

```powershell
wxemo completion powershell | Out-String | Invoke-Expression
```

需要持久生效时, 将这行加入自己的 PowerShell profile.

## Elvish

在 Elvish 中运行, 当前会话即生效:

```elvish
eval (wxemo completion elvish | slurp)
```
