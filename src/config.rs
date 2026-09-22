//! 设置：`--qemu` / `AOSC_EXEC_GUARD_QEMU` / 用户配置 / `/etc` 系统默认的优先级与读写。

use std::env;
use std::path::{Path, PathBuf};

use clap::ValueEnum;

use crate::i18n::lang;
use crate::platform::in_chroot;
use crate::qemu::registry_visible;

/// 检测到匹配的 qemu-user 条目时怎么办。
///
/// 变体上的 doc 注释会变成 clap 的取值说明（clap 只会照列其取值、前缀是写死的英文
/// possible values，换不了语言），所以变体上不写 doc 注释。
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum QemuMode {
    Ask,
    Always,
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

/// 优先级：命令行 > 环境变量 > 用户配置 > `/etc` 系统默认（后两者是“不再询问”
/// 记住的选择；系统默认留给打包方/管理员放全机策略，用户配置能盖过它）。
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

/// 用户配置：`$XDG_CONFIG_HOME/aosc-exec-guard.conf`（一般是 `~/.config/…`）。
fn user_config_path() -> Option<PathBuf> {
    Some(dirs::config_dir()?.join("aosc-exec-guard.conf"))
}

/// 系统级默认：`/etc/aosc-exec-guard.conf`（打包方/管理员放全机策略，用户能盖过）。
pub const SYSTEM_CONFIG: &str = "/etc/aosc-exec-guard.conf";

fn system_config_path() -> PathBuf {
    // 测试钩子：无 root 的测试写不了 /etc。
    env::var_os("AOSC_EXEC_GUARD_SYSTEM_CONFIG")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(SYSTEM_CONFIG))
}

/// 读一个 conf 文件里的 `qemu = …`（`#` 起注释）。
fn read_qemu_mode(path: &Path) -> Option<QemuMode> {
    let text = std::fs::read_to_string(path).ok()?;
    text.lines().find_map(|line| {
        let line = line.split('#').next().unwrap_or("").trim();
        let (key, value) = line.split_once('=')?;
        (key.trim() == "qemu")
            .then(|| parse_qemu_mode(value))
            .flatten()
    })
}

/// “不再询问”记住的选择：用户配置优先，其次 `/etc` 里的系统默认。
fn load_saved_qemu_mode() -> Option<QemuMode> {
    user_config_path()
        .and_then(|path| read_qemu_mode(&path))
        .or_else(|| read_qemu_mode(&system_config_path()))
}

pub fn save_qemu_mode(mode: QemuMode) -> std::io::Result<()> {
    let l = lang();
    let path = user_config_path().ok_or_else(|| std::io::Error::other(l.no_config_dir()))?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let value = match mode {
        QemuMode::Ask => "ask",
        QemuMode::Always => "always",
        QemuMode::Never => "never",
    };
    std::fs::write(path, l.user_config_content(value))
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

    #[test]
    fn reads_qemu_mode_from_a_conf_file() {
        let path =
            env::temp_dir().join(format!("aosc-exec-guard-test-{}.conf", std::process::id()));
        std::fs::write(&path, "# 注释\nqemu = never # 行内注释\n").unwrap();
        assert_eq!(read_qemu_mode(&path), Some(QemuMode::Never));
        std::fs::write(&path, "qemu = sometimes\n").unwrap();
        assert_eq!(read_qemu_mode(&path), None);
        std::fs::remove_file(&path).unwrap();
        assert_eq!(read_qemu_mode(&path), None);
    }
}
