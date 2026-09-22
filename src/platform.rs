//! 运行环境判定：用哪种界面说话（图形 / 终端）、是不是 systemd 服务、
//! 在不在 chroot / 容器里。

use std::env;
use std::io::IsTerminal;
use std::path::Path;

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
/// 判据是照 systemd-detect-virt 抄一份的，不调那个命令：chroot / 目标系统里
/// 不一定有它，也未必是 systemd 系统。任一条命中就算容器：
/// - 宿主上 2 号进程永远是内核线程 `kthreadd`；容器（新的 PID namespace）里
///   `/proc/2` 是普通进程。读不到 /proc 或没权限（hidepid）时不当信号——
///   宁可漏报，那种情况注册表多半也看不见，调用方会兜底；
/// - 自己环境里的 `container=`（容器里的 systemd 会给所有进程带上）；
/// - 容器管理器留下的标记文件。
pub fn in_container() -> bool {
    // 测试钩子：无 root 的测试造不出真容器。
    if env_switch("AOSC_EXEC_GUARD_FORCE_CONTAINER") {
        return true;
    }

    if env::var_os("container").is_some() {
        return true;
    }

    let markers = [
        "/run/host/container-manager", // systemd-nspawn / lxc 等的容器管理接口
        "/run/systemd/container",      // 容器里的 systemd 写下的探测结果
        "/.dockerenv",                 // docker
        "/run/.containerenv",          // podman
    ];

    if markers.iter().any(|path| Path::new(path).exists()) {
        return true;
    }

    std::fs::read_to_string("/proc/2/comm").is_ok_and(|comm| comm.trim() != "kthreadd")
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
