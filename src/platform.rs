//! 运行环境判定：用哪种界面说话（图形 / 终端）、是不是 systemd 服务、
//! 在不在 chroot 里。

use std::env;
use std::io::IsTerminal;
use std::process::{Command, Stdio};

/// 环境变量开关按「存在即开启」处理：内核调用时没法给 guard 传选项。
pub fn env_switch(key: &str) -> bool {
    env::var_os(key).is_some()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DisplayMode {
    Dialog,
    Text,
}

pub fn gui_available() -> bool {
    let set = |key: &str| env::var_os(key).is_some_and(|v| !v.is_empty());
    set("DISPLAY") || set("WAYLAND_DISPLAY")
}

pub fn in_terminal() -> bool {
    std::io::stdout().is_terminal() || std::io::stderr().is_terminal()
}

/// systemd unit context: never block on an interactive dialog.
pub fn in_service() -> bool {
    env::var_os("INVOCATION_ID").is_some()
}

/// 本进程是在 chroot（或 pivot_root）里吗：根目录和 PID 1 的根不是同一个。
///
/// 没挂 /proc（读不到 `/proc/1/root`）时无从判断，当作“没在 chroot 里”。
pub fn in_chroot() -> bool {
    use std::os::unix::fs::MetadataExt;
    // 测试钩子：无 root 的测试造不出真 chroot（判据就是 /proc/1/root）。
    if env_switch("AOSC_EXEC_GUARD_FORCE_CHROOT") {
        return true;
    }
    let (Ok(here), Ok(pid1)) = (std::fs::metadata("/"), std::fs::metadata("/proc/1/root")) else {
        return false;
    };
    (here.dev(), here.ino()) != (pid1.dev(), pid1.ino())
}

/// 本进程在容器（嵌套的 PID namespace）里吗——容器里 `/proc/1` 是容器自己的
/// init，借不到宿主机的注册表（见 qemu::find_host_qemu_entry）。
///
/// 用 `systemd-detect-virt` 判定：实测它只在 PID namespace 嵌套时报“是容器”
/// （单独 unshare 挂载/网络 namespace 都报 none），正合这里的需要；
/// 命令不可用（非 systemd 系统、连 /usr 都没有的裸容器）时当作不是——那种
/// 情况下注册表多半也看不见，调用方靠 `registry_visible()` 兜底。
pub fn in_container() -> bool {
    // 测试钩子：无 root 的测试造不出真容器。
    if env_switch("AOSC_EXEC_GUARD_FORCE_CONTAINER") {
        return true;
    }
    Command::new("systemd-detect-virt")
        .args(["--quiet", "--container"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

pub fn decide_mode(no_dialog: bool) -> DisplayMode {
    if no_dialog {
        return DisplayMode::Text;
    }

    if in_service() || !gui_available() || in_terminal() {
        return DisplayMode::Text;
    }

    DisplayMode::Dialog
}
