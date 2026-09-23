//! `--handover`：把位置让给内核的 qemu 条目。
//!
//! guard 挡在条目最前面，但它自己转发需要“脚下能看见的模拟器”：chroot 没挂
//! /proc、容器里都借不到宿主注册表，而内核本来能直接跑 qemu 条目（带 `F` 的
//! 条目用的是注册时打开的宿主机解释器，rootfs / 容器里根本不需要放 qemu）。
//! 于是“装了 guard 反而要往里塞 qemu”，白白倒退。
//!
//! 让位 = 注销 guard 的 binfmt 条目 + 把安装时写下的 conf 改名成 `.disabled`
//! （systemd-binfmt 只认 `.conf`，重启后不再注册）。之后外架构程序直接交给
//! 内核的 qemu 条目：宿主机、chroot、容器 行为统一。代价是 guard 的询问和解释
//! 不再出现（见 README）。
//!
//! 只能在宿主机（外面）上做：chroot 里注册表属于宿主机、可配置文件属于 chroot；
//! 容器（PID namespace）里注册表往往压根借不到——两种情况都拒绝并提示到外面跑。

use std::env;
use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context as _, bail};
use clap::ValueEnum;
use dialoguer::Confirm;
use dialoguer::theme::ColorfulTheme;
use rust_i18n::t;

use crate::platform::{in_chroot, in_container};
use crate::qemu::{binfmt_dir, enabled_qemu_entries, guard_entries, registry_visible};

/// `--handover` 的动作。
///
/// 变体上的 doc 注释会变成 clap 的取值说明（clap 只会照列其取值、前缀是写死的英文
/// possible values，换不了语言），所以变体上不写 doc 注释。
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum Action {
    On,
    Off,
}

/// 安装脚本写下的 conf 名（scripts/install.sh）。
const CONF_NAME: &str = "zz-aosc-exec-guard.conf";
/// 让位时改成这个名字：systemd-binfmt 只认 `.conf`，不会再应用。
const DISABLED_SUFFIX: &str = ".disabled";
/// systemd-binfmt 会读的目录（照它就对了）。
const CONF_DIRS: &[&str] = &[
    "/etc/binfmt.d",
    "/run/binfmt.d",
    "/usr/local/lib/binfmt.d",
    "/usr/lib/binfmt.d",
];

pub fn run(action: Action, assume_yes: bool) -> i32 {
    let result = match action {
        Action::On => hand_over(assume_yes),
        Action::Off => take_back(assume_yes),
    };
    match result {
        Ok(()) => 0,
        Err(err) => {
            // `{:#}` 展开 anyhow 的 context 链（本模块的文案大多是 context）。
            eprintln!("aosc-exec-guard: {err:#}");
            1
        }
    }
}

fn hand_over(assume_yes: bool) -> anyhow::Result<()> {
    check_host_context()?;
    let dir = binfmt_dir();
    let (installed, disabled) = conf_files();
    let entries = guard_entries(&dir);

    if installed.is_empty() && entries.is_empty() {
        if disabled.is_empty() {
            bail!("{}", t!("ho-nothing-installed", conf_name = CONF_NAME));
        } else {
            bail!(
                "{}",
                t!(
                    "ho-already",
                    conf_name = CONF_NAME,
                    suffix = DISABLED_SUFFIX
                )
            );
        }
    }

    let qemu = enabled_qemu_entries(&dir);
    if qemu.is_empty() {
        eprintln!("aosc-exec-guard: {}", t!("ho-no-qemu-warning"));
    }
    if installed.is_empty() {
        eprintln!(
            "aosc-exec-guard: {}",
            t!("ho-no-conf-notice", conf_name = CONF_NAME)
        );
    }

    let question = if qemu.is_empty() {
        t!("ho-question-no-qemu")
    } else {
        t!("ho-question")
    };
    if !assume_yes && !confirm(&question)? {
        println!("aosc-exec-guard: {}", t!("cancelled"));
        return Ok(());
    }

    for path in &installed {
        let target = disabled_path(path);
        std::fs::rename(path, &target)
            .with_context(|| t!("ho-disable-failed", path = path.display().to_string()))?;
        println!("{}", t!("ho-disabled", path = path.display().to_string()));
    }
    for name in &entries {
        unregister(&dir, name)?;
        println!("{}", t!("ho-unregistered", name = name));
    }

    let left = guard_entries(&dir);
    if !left.is_empty() {
        bail!(
            "{}",
            t!("ho-residue", list = left.join(&t!("list-separator")))
        );
    }

    println!("{}", t!("ho-done"));
    println!("{}", t!("ho-restore-hint"));

    Ok(())
}

fn take_back(assume_yes: bool) -> anyhow::Result<()> {
    check_host_context()?;
    let dir = binfmt_dir();
    let (_, disabled) = conf_files();
    if disabled.is_empty() {
        bail!(
            "{}",
            t!(
                "tb-not-handed",
                conf_name = CONF_NAME,
                suffix = DISABLED_SUFFIX
            )
        );
    }
    if !assume_yes && !confirm(&t!("tb-question"))? {
        println!("aosc-exec-guard: {}", t!("cancelled"));
        return Ok(());
    }

    for path in &disabled {
        let Some(conf) = enabled_path(path) else {
            continue;
        };
        std::fs::rename(path, &conf)
            .with_context(|| t!("tb-restore-failed", path = path.display().to_string()))?;
        println!("{}", t!("tb-restored", path = conf.display().to_string()));
    }

    if env::var_os("AOSC_EXEC_GUARD_BINFMT_DIR").is_some() {
        println!("{}", t!("tb-test-mode"));
        return Ok(());
    }
    restart_binfmt()?;
    let entries = guard_entries(&dir);
    if entries.is_empty() {
        bail!("{}", t!("tb-verify-failed"));
    }
    println!("{}", t!("tb-done", count = entries.len()));
    Ok(())
}

/// 让位必须做在宿主机上：注册表、配置文件、以及重启后的重放得属于同一个系统。
fn check_host_context() -> anyhow::Result<()> {
    if in_chroot() {
        bail!("{}", t!("ctx-chroot"));
    }
    if in_container() {
        bail!("{}", t!("ctx-container"));
    }
    if !registry_visible() {
        bail!("{}", t!("ctx-no-registry"));
    }
    Ok(())
}

fn confirm(question: &str) -> anyhow::Result<bool> {
    if !std::io::stdin().is_terminal() || !std::io::stderr().is_terminal() {
        bail!("{}", t!("confirm-needs-tty"));
    }
    // 和终端菜单同一套观感（dialoguer 自带的 ColorfulTheme）。
    let theme = ColorfulTheme::default();
    Confirm::with_theme(&theme)
        .with_prompt(question)
        .default(false)
        .interact()
        .context(t!("confirm-failed"))
}

/// 注销注册表里的一个条目：写 `-1`（内核语义）。测试目录（普通文件）里写完再
/// 删掉文件；真实的 procfs 里内核会把文件本身收走。
fn unregister(dir: &Path, name: &str) -> anyhow::Result<()> {
    let path = dir.join(name);
    std::fs::write(&path, "-1").with_context(|| t!("ho-unregister-failed", name = name))?;
    if env::var_os("AOSC_EXEC_GUARD_BINFMT_DIR").is_some() {
        let _ = std::fs::remove_file(&path);
    }
    Ok(())
}

/// 重启 systemd-binfmt，让它按 conf 重新注册条目。
fn restart_binfmt() -> anyhow::Result<()> {
    // 先清掉 systemd 的开始频率限制：restart 的 stop 阶段会注销所有条目，
    // 若 start 被限流挡住就什么都不剩（和 scripts/install.sh 同款处理）。
    let _ = Command::new("systemctl")
        .args(["reset-failed", "systemd-binfmt.service"])
        .status();
    let status = Command::new("systemctl")
        .args(["restart", "systemd-binfmt.service"])
        .status()
        .context(t!("restart-failed"))?;
    if !status.success() {
        bail!("{}", t!("restart-status-failed"));
    }
    Ok(())
}

///（还在用的 conf, 已停用的 conf），按 [`CONF_DIRS`] 顺序找。
fn conf_files() -> (Vec<PathBuf>, Vec<PathBuf>) {
    let mut installed = Vec::new();
    let mut disabled = Vec::new();
    for dir in conf_dirs() {
        let conf = dir.join(CONF_NAME);
        if conf.exists() {
            installed.push(conf);
        }
        let off = dir.join(format!("{CONF_NAME}{DISABLED_SUFFIX}"));
        if off.exists() {
            disabled.push(off);
        }
    }
    (installed, disabled)
}

/// binfmt.d 目录；测试可用 AOSC_EXEC_GUARD_CONF_DIRS 指到临时目录（冒号分隔）。
fn conf_dirs() -> Vec<PathBuf> {
    match env::var_os("AOSC_EXEC_GUARD_CONF_DIRS") {
        Some(value) => env::split_paths(&value).collect(),
        None => CONF_DIRS.iter().map(PathBuf::from).collect(),
    }
}

fn disabled_path(conf: &Path) -> PathBuf {
    let mut name = conf.file_name().unwrap_or_default().to_os_string();
    name.push(DISABLED_SUFFIX);
    conf.with_file_name(name)
}

fn enabled_path(disabled: &Path) -> Option<PathBuf> {
    let name = disabled.file_name()?.to_str()?;
    Some(disabled.with_file_name(name.strip_suffix(DISABLED_SUFFIX)?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disabled_suffix_round_trips() {
        let conf = Path::new("/usr/lib/binfmt.d/zz-aosc-exec-guard.conf");
        let disabled = disabled_path(conf);
        assert_eq!(
            disabled,
            Path::new("/usr/lib/binfmt.d/zz-aosc-exec-guard.conf.disabled")
        );
        assert_eq!(enabled_path(&disabled).as_deref(), Some(conf));
        assert!(enabled_path(conf).is_none());
    }
}
