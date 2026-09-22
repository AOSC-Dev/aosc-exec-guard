//! 设置：`--qemu` / `AOSC_EXEC_GUARD_QEMU` / 用户配置文件的优先级与读写。

use std::env;
use std::path::PathBuf;

use clap::ValueEnum;

use crate::platform::in_chroot;
use crate::qemu::registry_visible;

/// 检测到匹配的 qemu-user 条目时怎么办。
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum QemuMode {
    /// 先询问（默认）
    Ask,
    /// 总是直接交给仿真器运行
    Always,
    /// 从不运行，只做解释
    Never,
}

pub fn parse_qemu_mode(value: &str) -> Option<QemuMode> {
    match value.trim() {
        "ask" => Some(QemuMode::Ask),
        "always" => Some(QemuMode::Always),
        "never" => Some(QemuMode::Never),
        _ => None,
    }
}

/// 优先级：命令行 > 环境变量 > 用户配置（“不再询问”记住的选择）。
///
/// chroot 里是个例外，而且“看不到 binfmt_misc 注册表”时也一样（没挂 /proc 的
/// chroot、容器里都算——那时根本没有可靠办法判断自己在哪）：不问、也不带
/// 宿主机保存的选择，默认直接交给模拟器，行为就跟没装 guard 时一样（让位）。
pub fn resolve_qemu_mode(cmdline: Option<QemuMode>) -> QemuMode {
    if let Some(mode) = cmdline {
        return mode;
    }
    if let Some(mode) = env::var_os("AOSC_EXEC_GUARD_QEMU")
        .and_then(|value| value.to_str().and_then(parse_qemu_mode))
    {
        return mode;
    }
    if in_chroot() || !registry_visible() {
        return QemuMode::Always;
    }
    load_saved_qemu_mode().unwrap_or(QemuMode::Ask)
}

/// 用户配置（`~/.config/aosc-exec-guard.conf`）。
fn config_path() -> Option<PathBuf> {
    let base = match env::var_os("XDG_CONFIG_HOME").filter(|dir| !dir.is_empty()) {
        Some(dir) => PathBuf::from(dir),
        None => PathBuf::from(env::var_os("HOME")?).join(".config"),
    };
    Some(base.join("aosc-exec-guard.conf"))
}

fn load_saved_qemu_mode() -> Option<QemuMode> {
    let text = std::fs::read_to_string(config_path()?).ok()?;
    text.lines().find_map(|line| {
        let line = line.split('#').next().unwrap_or("").trim();
        let (key, value) = line.split_once('=')?;
        (key.trim() == "qemu")
            .then(|| parse_qemu_mode(value))
            .flatten()
    })
}

pub fn save_qemu_mode(mode: QemuMode) -> std::io::Result<()> {
    let path =
        config_path().ok_or_else(|| std::io::Error::other("HOME 未设置，无法定位用户配置"))?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let value = match mode {
        QemuMode::Ask => "ask",
        QemuMode::Always => "always",
        QemuMode::Never => "never",
    };
    std::fs::write(
        path,
        format!(
            "# aosc-exec-guard 用户设置（“不再询问”时写入）\n\
             # qemu: ask=每次询问（默认）/ always=总是用仿真器运行 / never=从不运行\n\
             qemu = {value}\n"
        ),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn qemu_mode_parsing_and_precedence_strings() {
        assert_eq!(parse_qemu_mode("always"), Some(QemuMode::Always));
        assert_eq!(parse_qemu_mode(" never "), Some(QemuMode::Never));
        assert_eq!(parse_qemu_mode("ask"), Some(QemuMode::Ask));
        assert_eq!(parse_qemu_mode("sometimes"), None);
    }
}
