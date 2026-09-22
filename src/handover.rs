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

use clap::ValueEnum;
use dialoguer::Confirm;

use crate::platform::{in_chroot, in_container};
use crate::qemu::{binfmt_dir, enabled_qemu_entries, guard_entries, registry_visible};

/// `--handover` 的动作。
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum Action {
    /// 让位：注销 guard 的条目、停用它的配置文件
    On,
    /// 恢复：把配置文件改回来、重新注册条目
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
        Err(message) => {
            eprintln!("aosc-exec-guard: {message}");
            1
        }
    }
}

fn hand_over(assume_yes: bool) -> Result<(), String> {
    check_host_context()?;
    let dir = binfmt_dir();
    let (installed, disabled) = conf_files();
    let entries = guard_entries(&dir);

    if installed.is_empty() && entries.is_empty() {
        return if disabled.is_empty() {
            Err(format!(
                "没找到 guard 的 binfmt 条目，也没找到配置文件（{CONF_NAME}）：像是没装，无需让位。"
            ))
        } else {
            Err(format!(
                "guard 已经处于让位状态（{CONF_NAME}{DISABLED_SUFFIX} 还在）；要恢复用 `--handover=off`。"
            ))
        };
    }

    let qemu = enabled_qemu_entries(&dir);
    if qemu.is_empty() {
        eprintln!(
            "aosc-exec-guard: 警告：注册表里没有已启用的 qemu-* 条目——让位后外架构程序会直接\
             以 `Exec format error` 失败（没人解释，也没人模拟）。"
        );
    }
    if installed.is_empty() {
        eprintln!(
            "aosc-exec-guard: 提醒：没找到配置文件（{CONF_NAME}），只注销当前条目；\
             如果它是别的方式注册的，重启后可能回来。"
        );
    }

    let question = if qemu.is_empty() {
        "让 guard 退出（注销它的 binfmt 条目、停用配置文件）？注意：注册表里没有可用的 qemu 条目。"
    } else {
        "让 guard 退出（注销它的 binfmt 条目、停用配置文件），外架构程序改由内核的 qemu 条目直接处理？"
    };
    if !assume_yes && !confirm(question)? {
        println!("aosc-exec-guard: 已取消，什么都没改。");
        return Ok(());
    }

    for path in &installed {
        let target = disabled_path(path);
        std::fs::rename(path, &target)
            .map_err(|err| format!("停用 {} 失败（需要 root？）：{err}", path.display()))?;
        println!("已停用 {}", path.display());
    }
    for name in &entries {
        unregister(&dir, name)?;
        println!("已注销条目 {name}");
    }

    let left = guard_entries(&dir);
    if !left.is_empty() {
        return Err(format!("注销之后还有残留条目：{}", left.join("、")));
    }
    println!("让位完成：外架构程序现在由内核的 qemu 条目处理（宿主机、chroot、容器 都一样）。");
    println!("恢复：`sudo aosc-exec-guard --handover=off`，或重新运行安装脚本。");
    Ok(())
}

fn take_back(assume_yes: bool) -> Result<(), String> {
    check_host_context()?;
    let dir = binfmt_dir();
    let (_, disabled) = conf_files();
    if disabled.is_empty() {
        return Err(format!(
            "没有找到停用的配置文件（{CONF_NAME}{DISABLED_SUFFIX}）：当前不在让位状态。"
        ));
    }
    if !assume_yes && !confirm("恢复 guard：把配置文件改回来，并重新注册它的 binfmt 条目？")?
    {
        println!("aosc-exec-guard: 已取消，什么都没改。");
        return Ok(());
    }

    for path in &disabled {
        let Some(conf) = enabled_path(path) else {
            continue;
        };
        std::fs::rename(path, &conf)
            .map_err(|err| format!("恢复 {} 失败（需要 root？）：{err}", path.display()))?;
        println!("已恢复 {}", conf.display());
    }

    if env::var_os("AOSC_EXEC_GUARD_BINFMT_DIR").is_some() {
        println!("（测试模式：不重启 systemd-binfmt；条目会在重启或重装时回来。）");
        return Ok(());
    }
    restart_binfmt()?;
    let entries = guard_entries(&dir);
    if entries.is_empty() {
        return Err(
            "配置文件已恢复，但注册表里还没看到 guard 条目：看看 `systemctl status systemd-binfmt.service`。"
                .into(),
        );
    }
    println!("已恢复：guard 条目回来了（{} 个）。", entries.len());
    Ok(())
}

/// 让位必须做在宿主机上：注册表、配置文件、以及重启后的重放得属于同一个系统。
fn check_host_context() -> Result<(), String> {
    if in_chroot() {
        return Err(
            "看起来在 chroot 里：注册表属于宿主机、配置文件属于 chroot，请在宿主机（外面）上运行。"
                .into(),
        );
    }
    if in_container() {
        return Err("看起来在容器里（嵌套的 PID namespace）：请在宿主机（外面）上运行。".into());
    }
    if !registry_visible() {
        return Err("看不到 binfmt_misc 注册表：请在宿主机（外面）上运行。".into());
    }
    Ok(())
}

fn confirm(question: &str) -> Result<bool, String> {
    if !std::io::stdin().is_terminal() || !std::io::stderr().is_terminal() {
        return Err("需要确认，但这里没有终端：请加 --yes（或到终端里运行）。".into());
    }
    Confirm::new()
        .with_prompt(question)
        .default(false)
        .interact()
        .map_err(|err| format!("询问失败：{err}"))
}

/// 注销注册表里的一个条目：写 `-1`（内核语义）。测试目录（普通文件）里写完再
/// 删掉文件；真实的 procfs 里内核会把文件本身收走。
fn unregister(dir: &Path, name: &str) -> Result<(), String> {
    let path = dir.join(name);
    std::fs::write(&path, "-1")
        .map_err(|err| format!("注销条目 {name} 失败（需要 root？）：{err}"))?;
    if env::var_os("AOSC_EXEC_GUARD_BINFMT_DIR").is_some() {
        let _ = std::fs::remove_file(&path);
    }
    Ok(())
}

/// 重启 systemd-binfmt，让它按 conf 重新注册条目。
fn restart_binfmt() -> Result<(), String> {
    // 先清掉 systemd 的开始频率限制：restart 的 stop 阶段会注销所有条目，
    // 若 start 被限流挡住就什么都不剩（和 scripts/install.sh 同款处理）。
    let _ = Command::new("systemctl")
        .args(["reset-failed", "systemd-binfmt.service"])
        .status();
    let status = Command::new("systemctl")
        .args(["restart", "systemd-binfmt.service"])
        .status()
        .map_err(|err| {
            format!(
                "没法运行 systemctl（{err}）：请手动 `systemctl restart systemd-binfmt.service`。"
            )
        })?;
    if !status.success() {
        return Err("`systemctl restart systemd-binfmt.service` 失败：请手动重启后再看。".into());
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
