//! 多语言：内建 zh_CN / en 文案，编译进二进制（chroot / 空 rootfs 里也能用，
//! 不依赖 /usr/share/locale）。
//!
//! 语言在启动时按一次环境变量决定：
//! `AOSC_EXEC_GUARD_LANG` > `LC_ALL` > `LC_MESSAGES` > `LANG`；
//! `zh*` 用中文，`en*` / `C` / `POSIX` / 其它明确的 locale 用英文；
//! 一个都没设（服务、chroot 里很常见）默认中文。
//!
//! 要加一门语言：给下面 `fixed!` / `text!` 两张消息表各补一列（宏定义也在文件里），
//! 再把 [`Lang`] 的判定分支补上即可。

use std::env;
use std::io;
use std::sync::OnceLock;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lang {
    ZhCn,
    En,
}

/// 当前语言（进程里只判定一次；命令行工具没必要变来变去）。
pub fn lang() -> Lang {
    static LANG: OnceLock<Lang> = OnceLock::new();
    *LANG.get_or_init(|| {
        let value = env::var_os("AOSC_EXEC_GUARD_LANG").or_else(|| {
            ["LC_ALL", "LC_MESSAGES", "LANG"]
                .iter()
                .find_map(env::var_os)
        });
        match value {
            Some(value) => Lang::from_locale(&value.to_string_lossy()),
            None => Lang::ZhCn,
        }
    })
}

impl Lang {
    /// `zh_CN.UTF-8` / `en_US` / `C` → 看语言部分；认不出来的一律英文。
    fn from_locale(locale: &str) -> Lang {
        let lower = locale.to_ascii_lowercase();
        let name = lower.split(['.', '@']).next().unwrap_or("");
        if name.starts_with("zh") {
            Lang::ZhCn
        } else {
            Lang::En
        }
    }

    /// ELF 解析失败的原因：枚举 → 文案（“按参数选文案”，纯表表达不了，单独写）。
    pub fn not_elf_reason(&self, reason: crate::elf::NotElfReason) -> &'static str {
        use crate::elf::NotElfReason as R;
        let (zh, en) = match reason {
            R::CannotRead => ("无法读取该文件", "cannot read the file"),
            R::TooSmall => (
                "文件太小，不是一个有效的 ELF 可执行文件",
                "file is too small to be a valid ELF executable",
            ),
            R::NotElf => ("文件不是 ELF 可执行文件", "file is not an ELF executable"),
            R::BadClass => ("ELF 头部的位宽字段无法识别", "unrecognized ELF class field"),
            R::BadEndian => (
                "ELF 头部的字节序字段无法识别",
                "unrecognized ELF endianness field",
            ),
            R::ReadFailed => ("读取文件失败", "failed to read the file"),
        };
        match self {
            Lang::ZhCn => zh,
            Lang::En => en,
        }
    }
}

/// 固定文案（没占位）：一行一条，宏生成 `pub fn …(&self) -> &'static str`。
macro_rules! fixed {
    ($($name:ident => ($zh:literal, $en:literal);)*) => {
        impl Lang {
            $(
                pub fn $name(&self) -> &'static str {
                    match self {
                        Lang::ZhCn => $zh,
                        Lang::En => $en,
                    }
                }
            )*
        }
    };
}

/// 带占位符的文案：一行一条，宏生成 `pub fn …(&self, …) -> String`；
/// 参数名要和 `{…}` 里的名字一致（就是 `format!` 的内联捕获）。
macro_rules! text {
    ($($name:ident($($arg:ident: $ty:ty),*) => ($zh:literal, $en:literal);)*) => {
        impl Lang {
            $(
                pub fn $name(&self $(, $arg: $ty)*) -> String {
                    match self {
                        Lang::ZhCn => format!($zh),
                        Lang::En => format!($en),
                    }
                }
            )*
        }
    };
}

fixed! {
    // ---------- 通用 ----------
    dialog_title => ("无法运行此程序", "Cannot run this program");

    // ---------- ELF 解释 ----------
    hint_isolated => ("提示：看起来在 chroot / 容器 里（或者看不到 binfmt_misc 注册表）：这里找不到能用的模拟器条目，guard 转不了。如果宿主机装了 qemu-user：到外面（宿主机）上运行 `sudo aosc-exec-guard --handover`，让 guard 退出、由内核的 qemu 条目接管——qemu 条目用 F 直接打开宿主机的解释器，chroot / 容器 里不需要放 qemu。",
                      "Hint: this looks like a chroot / container (or the binfmt_misc registry is not visible): there is no usable emulator entry here, so the guard cannot forward. If qemu-user is installed on the host, run `sudo aosc-exec-guard --handover` outside: the guard steps aside and the kernel's qemu entries take over - they use F to open the host interpreter, so no qemu needs to exist inside the chroot / container.");
    hint_install_emulator => ("提示：可以安装对应架构的模拟器（qemu-user-static、box64 等）后重试，或改用 AOSC OS 原生版本。",
                              "Hint: install an emulator for that architecture (qemu-user-static, box64, ...) and retry, or use the native AOSC OS build.");

    // ---------- 询问 ----------
    menu_run_once => ("运行（这次）", "Run (once)");
    menu_decline => ("不运行（这次）", "Don't run (once)");
    menu_always => ("总是运行（不再询问）", "Always run (don't ask again)");
    run_label => ("运行", "Run");
    decline_label => ("不运行", "Don't run");
    dont_ask_again => ("不再询问", "Don't ask again");

    // ---------- --handover ----------
    ho_no_qemu_warning => ("警告：注册表里没有已启用的 qemu-* 条目——让位后外架构程序会直接以 `Exec format error` 失败（没人解释，也没人模拟）。",
                           "warning: no enabled qemu-* entry in the registry - after the handover foreign binaries will simply fail with `Exec format error` (nothing explains, nothing emulates).");
    ho_question => ("让 guard 退出（注销它的 binfmt 条目、停用配置文件），外架构程序改由内核的 qemu 条目直接处理？",
                    "Let the guard step aside (unregister its binfmt entries, disable its config file) so the kernel's qemu entries handle foreign binaries directly?");
    ho_question_no_qemu => ("让 guard 退出（注销它的 binfmt 条目、停用配置文件）？注意：注册表里没有可用的 qemu 条目。",
                            "Let the guard step aside (unregister its binfmt entries, disable its config file)? Note: there is no usable qemu entry in the registry.");
    cancelled => ("已取消，什么都没改。", "cancelled, nothing changed.");
    ho_done => ("让位完成：外架构程序现在由内核的 qemu 条目处理（宿主机、chroot、容器 都一样）。",
                "handover done: foreign binaries are now handled by the kernel's qemu entries (host, chroot and containers alike).");
    ho_restore_hint => ("恢复：`sudo aosc-exec-guard --handover=off`，或重新运行安装脚本。",
                        "to restore: `sudo aosc-exec-guard --handover=off`, or rerun the installer.");
    tb_question => ("恢复 guard：把配置文件改回来，并重新注册它的 binfmt 条目？",
                    "Restore the guard (rename its config file back and re-register its binfmt entries)?");
    tb_test_mode => ("（测试模式：不重启 systemd-binfmt；条目会在重启或重装时回来。）",
                     "(test mode: systemd-binfmt not restarted; entries return on reboot or reinstall.)");
    tb_verify_failed => ("配置文件已恢复，但注册表里还没看到 guard 条目：看看 `systemctl status systemd-binfmt.service`。",
                         "config file restored, but no guard entry in the registry yet: check `systemctl status systemd-binfmt.service`.");
    ctx_chroot => ("看起来在 chroot 里：注册表属于宿主机、配置文件属于 chroot，请在宿主机（外面）上运行。",
                   "this looks like a chroot: the registry belongs to the host while the config files belong to the chroot - run this on the host (outside).");
    ctx_container => ("看起来在容器里（嵌套的 PID namespace）：请在宿主机（外面）上运行。",
                      "this looks like a container (nested PID namespace): run this on the host (outside).");
    ctx_no_registry => ("看不到 binfmt_misc 注册表：请在宿主机（外面）上运行。",
                        "the binfmt_misc registry is not visible: run this on the host (outside).");
    confirm_needs_tty => ("需要确认，但这里没有终端：请加 --yes（或到终端里运行）。",
                          "confirmation needed, but there is no terminal here: pass --yes (or run it in a terminal).");
    restart_status_failed => ("`systemctl restart systemd-binfmt.service` 失败：请手动重启后再看。",
                              "`systemctl restart systemd-binfmt.service` failed: please restart it manually and check.");

    // ---------- 用户配置文件 ----------
    no_config_dir => ("无法定位用户配置目录（HOME / XDG_CONFIG_HOME）",
                      "cannot locate the user config dir (HOME / XDG_CONFIG_HOME)");

    // ---------- 命令行 / --help（由 main.rs 的 clap 属性在运行时取用）----------
    cli_about => ("说明一个程序为何无法在本机运行（binfmt_misc 解释器）。",
                  "explain why a program cannot run on this machine (binfmt_misc interpreter).");
    cli_usage => ("aosc-exec-guard [选项] <程序路径> [参数…]",
                  "aosc-exec-guard [OPTIONS] <PROGRAM> [ARGS…]");
    // clap 的模板：把默认的 “Usage:” 标题换成自己的（其余占位符不动）。
    cli_help_template => ("{before-help}{about-with-newline}\n用法: {usage}\n\n{all-args}{after-help}",
                          "{before-help}{about-with-newline}\nUsage: {usage}\n\n{all-args}{after-help}");
    cli_arguments_heading => ("参数", "Arguments");
    cli_options_heading => ("选项", "Options");
    cli_after_help => ("（通常由内核通过 binfmt_misc 调用，无需手动运行；`--handover` 例外——要手动以 root 运行。）",
                       "(normally invoked by the kernel through binfmt_misc; only `--handover` is meant to be run by hand, as root.)");
    cli_path_value => ("程序路径", "PROGRAM");
    cli_mode_value => ("模式", "MODE");
    cli_action_value => ("动作", "ACTION");
    cli_path_help => ("无法运行的程序路径（其后的参数原属于原程序）",
                      "path of the program that cannot run (the arguments after it belong to the program)");
    cli_no_dialog_help => ("永远不弹框（相当于 AOSC_EXEC_GUARD_NO_DIALOG=1）",
                           "never open a dialog (same as AOSC_EXEC_GUARD_NO_DIALOG=1)");
    cli_debug_help => ("打印模式判定等调试信息（含 binfmt 注册表可见性；相当于 AOSC_EXEC_GUARD_DEBUG=1）",
                       "print the decision and other debug info (incl. binfmt registry visibility; same as AOSC_EXEC_GUARD_DEBUG=1)");
    // 取值列表由 clap 自己附在帮助后面（跟 oma 一样，那一小段是英文的）。
    cli_qemu_help => ("检测到 qemu-user 仿真器时怎么办（环境变量 AOSC_EXEC_GUARD_QEMU；默认读保存过的选择）",
                      "what to do when a qemu-user emulator is detected (env AOSC_EXEC_GUARD_QEMU; defaults to the saved choice)");
    cli_handover_help => ("让 guard 退出、把外架构程序交给内核的 qemu 条目（--handover=off 恢复）",
                          "step aside so the kernel's qemu entries handle foreign binaries (--handover=off to restore)");
    cli_yes_help => ("跳过确认询问（非交互环境必须加；只对 --handover 有意义）",
                     "skip the confirmation prompt (required when non-interactive; only meaningful with --handover)");
    cli_help_help => ("显示帮助信息", "Print help");
    cli_version_help => ("显示版本号", "Print version");
}

text! {
    // ---------- 通用 ----------
    cannot_start(entry: &str, err: &io::Error) => ("无法启动 {entry}：{err}", "cannot start {entry}: {err}");
    cannot_remember(err: &io::Error) => ("无法记住“不再询问”的选择：{err}", "could not remember the \"don't ask again\" choice: {err}");

    // ---------- ELF 解释 ----------
    unknown_machine(machine: u16) => ("未知架构（e_machine=0x{machine:x}）", "unknown architecture (e_machine=0x{machine:x})");
    arch_sentence(machine: &str, bits: u32, native: &str) => ("该程序是为 {machine}构建的 {bits} 位程序，而本机是 {native}", "a {bits}-bit program built for {machine}, but this machine is {native}");
    hint_emulator_installed(entry: &str) => ("提示：本机已安装 {entry}，设置 AOSC_EXEC_GUARD_QEMU=always 可让它自动运行（ask=每次询问、never=从不运行）。", "Hint: {entry} is installed; set AOSC_EXEC_GUARD_QEMU=always to run it automatically (ask=ask every time, never=never run).");
    cannot_run(path: &str, why: &str, hint: &str) => ("无法运行“{path}”：{why}。\n{hint}", "\"{path}\" cannot run: {why}.\n{hint}");
    native_but_rejected(path: &str, machine: &str) => ("无法运行“{path}”：程序架构与本机一致（{machine}），但内核拒绝了它，文件可能已损坏或格式不受支持。", "\"{path}\" cannot run: it matches this machine ({machine}), but the kernel rejected it - the file may be corrupted or in an unsupported format.");
    cannot_run_notelf(path: &str, why: &str) => ("无法运行“{path}”：{why}。", "\"{path}\" cannot run: {why}.");

    // ---------- 询问 ----------
    qemu_question(path: &str, sentence: &str) => ("“{path}”不能在本机直接运行：{sentence}。", "\"{path}\" cannot run on this machine directly: {sentence}.");
    run_with_prompt(entry: &str) => ("用 {entry} 运行吗", "Run with {entry}?");

    // ---------- --handover ----------
    ho_nothing_installed(conf_name: &str) => ("没找到 guard 的 binfmt 条目，也没找到配置文件（{conf_name}）：像是没装，无需让位。", "no guard binfmt entry and no config file ({conf_name}) found: nothing to hand over.");
    ho_already(conf_name: &str, suffix: &str) => ("guard 已经处于让位状态（{conf_name}{suffix} 还在）；要恢复用 `--handover=off`。", "the guard is already handed over ({conf_name}{suffix} is still there); use `--handover=off` to restore.");
    ho_no_conf_notice(conf_name: &str) => ("提醒：没找到配置文件（{conf_name}），只注销当前条目；如果它是别的方式注册的，重启后可能回来。", "note: no config file ({conf_name}) found; only the current entries are unregistered. If they were registered some other way they may come back after a reboot.");
    ho_disable_failed(path: &str, err: &io::Error) => ("停用 {path} 失败（需要 root？）：{err}", "could not disable {path} (root needed?): {err}");
    ho_disabled(path: &str) => ("已停用 {path}", "disabled {path}");
    ho_unregister_failed(name: &str, err: &io::Error) => ("注销条目 {name} 失败（需要 root？）：{err}", "could not unregister entry {name} (root needed?): {err}");
    ho_unregistered(name: &str) => ("已注销条目 {name}", "unregistered entry {name}");
    ho_residue(list: &str) => ("注销之后还有残留条目：{list}", "entries left behind after unregistering: {list}");
    tb_not_handed(conf_name: &str, suffix: &str) => ("没有找到停用的配置文件（{conf_name}{suffix}）：当前不在让位状态。", "no disabled config file ({conf_name}{suffix}) found: not in the handover state.");
    tb_restore_failed(path: &str, err: &io::Error) => ("恢复 {path} 失败（需要 root？）：{err}", "could not restore {path} (root needed?): {err}");
    tb_restored(path: &str) => ("已恢复 {path}", "restored {path}");
    tb_done(count: usize) => ("已恢复：guard 条目回来了（{count} 个）。", "restored: guard entries are back ({count}).");
    confirm_failed(err: &dyn std::fmt::Display) => ("询问失败：{err}", "prompt failed: {err}");
    restart_failed(err: &io::Error) => ("没法运行 systemctl（{err}）：请手动 `systemctl restart systemd-binfmt.service`。", "could not run systemctl ({err}): please restart manually with `systemctl restart systemd-binfmt.service`.");

    // ---------- 用户配置文件 ----------
    user_config_content(value: &str) => ("# aosc-exec-guard 用户设置（“不再询问”时写入）\n# qemu: ask=每次询问（默认）/ always=总是用仿真器运行 / never=从不运行\nqemu = {value}\n",
                                         "# aosc-exec-guard user settings (written when \"don't ask again\" was chosen)\n# qemu: ask=ask every time (default) / always=always run via the emulator / never=never run\nqemu = {value}\n");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn locale_names_map_to_a_language() {
        assert_eq!(Lang::from_locale("zh_CN.UTF-8"), Lang::ZhCn);
        assert_eq!(Lang::from_locale("zh_TW"), Lang::ZhCn);
        assert_eq!(Lang::from_locale("en_US.UTF-8"), Lang::En);
        assert_eq!(Lang::from_locale("en"), Lang::En);
        assert_eq!(Lang::from_locale("C"), Lang::En);
        assert_eq!(Lang::from_locale("POSIX"), Lang::En);
        assert_eq!(Lang::from_locale("de_DE.UTF-8"), Lang::En);
    }

    #[test]
    fn both_languages_have_text() {
        for lang in [Lang::ZhCn, Lang::En] {
            assert!(!lang.menu_run_once().is_empty());
            assert!(lang.cannot_run("/tmp/x", "why", "hint").contains("/tmp/x"));
        }
        assert!(Lang::ZhCn.ho_done().contains("让位"));
        assert!(Lang::En.ho_done().contains("handover"));
    }
}
