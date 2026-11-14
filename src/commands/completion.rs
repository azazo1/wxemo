//! 从 CLI 命令定义生成 shell 补全, 不读取账号数据.

use anyhow::{anyhow, Result};
use clap_complete::{env::Shells, Shell};

pub const COMPLETE_ENV: &str = "WXEMO_COMPLETE";

pub fn run(shell: Shell) -> Result<()> {
    // 使用运行时的命令树处理上下文和位置参数, 注册脚本仅调用 PATH 中的 wxemo.
    let shells = Shells::builtins();
    let completer = shells
        .completer(&shell.to_string())
        .ok_or_else(|| anyhow!("不支持的 shell: {shell}"))?;
    completer.write_registration(
        COMPLETE_ENV,
        "wxemo",
        "wxemo",
        "wxemo",
        &mut std::io::stdout().lock(),
    )?;
    Ok(())
}
