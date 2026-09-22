//! aosc-exec-guard — PoC: explain why an executable refused to start.
//!
//! Registered as a `binfmt_misc` interpreter (see `data/aosc-exec-guard.conf`),
//! the kernel runs this program when the native `binfmt_elf` loader rejects a
//! file with `-ENOEXEC` — typically a foreign-architecture ELF binary.
//!
//! It parses the ELF header, prints a human-readable explanation to stderr and
//! optionally shows a dialog (zenity/kdialog) when it was launched from a
//! graphical session. When a matching qemu-user binfmt entry is installed it can
//! also offer to run the program through the emulator instead — a GUI dialog, or
//! a menu on the terminal (see `--qemu`).
//! It exits with status 126, matching the shell convention for "cannot execute"
//! errors.
//!
//! 代码按职责分在几个模块里：`elf`（解析与判定）、`qemu`（模拟器发现与转发）、
//! `prompt`（询问界面）、`platform`（环境判定）、`config`（设置）、`handover`
//! （让位给内核的 qemu 条目）；这里只留命令行解析和主流程。

mod config;
mod elf;
mod handover;
mod i18n;
mod platform;
mod prompt;
mod qemu;

// 文案（locales/*.yml）编译期嵌进二进制；缺翻译回退英文。
rust_i18n::i18n!("locales", fallback = "en");

use std::env;
use std::ffi::OsString;
use std::path::Path;

use clap::{ArgAction, Parser};
use rust_i18n::t;

use crate::config::{QemuMode, resolve_qemu_mode, save_qemu_mode};
use crate::elf::{Verdict, build_message, classify, native_machine};
use crate::handover::Action;
use crate::platform::{
    DisplayMode, decide_mode, env_switch, gui_available, in_container, in_service, in_terminal,
};
use crate::prompt::{ask_run_via_qemu, show_dialog};
use crate::qemu::{find_qemu_entry, registry_visible, run_via_qemu};

/// Exit status for "found but cannot be executed" (shell convention).
const EXIT_CANNOT_EXEC: i32 = 126;

/// 说明一个程序为何无法在本机运行（binfmt_misc 解释器）。
///
/// 内核通过 binfmt_misc 调用时为 `aosc-exec-guard <程序路径> <原程序的参数…>`：
/// 路径之后的内容一律属于原程序，guard 只是原样接受、不当成自己的选项解析。
///
/// 用户可见的文字（含 `--help`）都在 `locales/*.yml`，属性里用 `t!(…)` 在
/// 运行时取（跟 oma 的 `fl!()` 一个路子）：clap 的 `-h/--help` 与 `-v/--version`
/// 是禁掉内置的、自己用本地化文案重加的；“用法:”标题与小节标题分别由
/// `help_template`、`next_help_heading` 提供。doc 注释只当开发文档用，clap 不会
/// 拿它渲染（`help`/`about` 都显式写了）。`t!` 给的是 `Cow`，clap 要 `String`，
/// 所以这里都 `.to_string()`。
#[derive(Debug, Parser)]
#[command(
    version,
    about = t!("cli-about").to_string(),
    long_about = None, // 别让上面的 doc 注释（开发文档）漏进帮助里
    override_usage = t!("cli-usage").to_string(),
    after_help = t!("cli-after-help").to_string(),
    help_template = t!("cli-help-template").to_string(),
    next_help_heading = t!("cli-options-heading").to_string(),
    disable_help_flag = true,
    disable_version_flag = true
)]
struct Cli {
    /// 无法运行的程序路径（其后的参数原属于原程序）
    #[arg(
        value_name = t!("cli-path-value").to_string(),
        help = t!("cli-path-help").to_string(),
        help_heading = t!("cli-arguments-heading").to_string(),
        required_unless_present = "handover",
        trailing_var_arg = true,
        allow_hyphen_values = true
    )]
    argv: Vec<OsString>,

    /// 永远不弹框（相当于 AOSC_EXEC_GUARD_NO_DIALOG=1）
    #[arg(long, help = t!("cli-no-dialog-help").to_string())]
    no_dialog: bool,

    /// 打印模式判定等调试信息（相当于 AOSC_EXEC_GUARD_DEBUG=1）
    #[arg(long, help = t!("cli-debug-help").to_string())]
    debug: bool,

    /// 检测到 qemu-user 仿真器时怎么办（环境变量 AOSC_EXEC_GUARD_QEMU）
    #[arg(
        long,
        value_enum,
        value_name = t!("cli-mode-value").to_string(),
        help = t!("cli-qemu-help").to_string()
    )]
    qemu: Option<QemuMode>,

    /// 让 guard 退出、把外架构程序交给内核的 qemu 条目
    #[arg(
        long,
        value_enum,
        value_name = t!("cli-action-value").to_string(),
        num_args = 0..=1,
        default_missing_value = "on",
        require_equals = true,
        help = t!("cli-handover-help").to_string()
    )]
    handover: Option<Action>,

    /// 跳过确认询问（只对 --handover 有意义）
    #[arg(long, help = t!("cli-yes-help").to_string())]
    yes: bool,

    /// 显示帮助信息
    #[arg(short, long, action = ArgAction::Help, help = t!("cli-help-help").to_string())]
    help: Option<bool>,

    /// 显示版本号
    #[arg(short, long, action = ArgAction::Version, help = t!("cli-version-help").to_string())]
    version: Option<bool>,
}

fn main() {
    i18n::init();
    let cli = Cli::parse();
    if let Some(action) = cli.handover {
        std::process::exit(handover::run(action, cli.yes));
    }
    let (target, program_args) = cli.argv.split_first().expect("clap 保证至少有一个程序路径");
    let target = Path::new(target);

    let no_dialog = cli.no_dialog || env_switch("AOSC_EXEC_GUARD_NO_DIALOG");
    let debug = cli.debug || env_switch("AOSC_EXEC_GUARD_DEBUG");
    let qemu_mode = resolve_qemu_mode(cli.qemu);

    let native = native_machine();
    let verdict = classify(target, native);
    let qemu_entry = match &verdict {
        Verdict::ArchMismatch(info) => find_qemu_entry(info),
        _ => None,
    };

    let mode = decide_mode(no_dialog);
    if debug {
        eprintln!(
            "[debug] mode={mode:?} qemu={} qemu_mode={qemu_mode:?} registry={} container={} gui={} tty={} in_service={} dialog={}",
            qemu_entry.as_ref().map_or("-", |entry| entry.name.as_str()),
            if registry_visible() {
                "visible"
            } else {
                "invisible"
            },
            in_container(),
            gui_available(),
            in_terminal(),
            in_service(),
            crate::prompt::dialog_chain(),
        );
    }

    if let Some(entry) = &qemu_entry
        && qemu_mode != QemuMode::Never
        && let Verdict::ArchMismatch(info) = &verdict
    {
        let run = match qemu_mode {
            QemuMode::Always => true,
            QemuMode::Ask => match ask_run_via_qemu(entry, target, info, mode) {
                Some(outcome) => {
                    if outcome.remember {
                        let remembered = if outcome.run {
                            QemuMode::Always
                        } else {
                            QemuMode::Never
                        };
                        if let Err(err) = save_qemu_mode(remembered) {
                            eprintln!("aosc-exec-guard: {:#}", err.context(t!("cannot-remember")));
                        }
                    }
                    outcome.run
                }
                // 没法询问（既没有图形界面，也没有终端）：保持“模拟器可用就直接交给它”的旧行为
                None => true,
            },
            QemuMode::Never => false,
        };
        if run {
            let err = run_via_qemu(entry, target, program_args);
            eprintln!(
                "aosc-exec-guard: {:#}",
                anyhow::Error::new(err).context(t!("cannot-start", entry = entry.name))
            );
        }
    }

    let message = build_message(target, env::consts::ARCH, &verdict, qemu_entry.as_ref());
    eprintln!("aosc-exec-guard: {message}");
    if mode == DisplayMode::Dialog {
        let _ = show_dialog(&t!("dialog-title"), &message);
    }
    std::process::exit(EXIT_CANNOT_EXEC);
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    /// `--help` 渲染出来也是本地化的（locale 是进程级的，这里钉住中文再对一遍）。
    #[test]
    fn help_uses_the_current_language() {
        let _pin = crate::i18n::pin("zh-CN");
        let mut command = Cli::command();
        let rendered = command.render_help().to_string();
        for expected in [
            t!("cli-about"),
            t!("cli-usage"),
            t!("cli-arguments-heading"),
            t!("cli-options-heading"),
            t!("cli-path-help"),
            t!("cli-help-help"),
            t!("cli-version-help"),
            t!("cli-after-help"),
        ] {
            assert!(
                rendered.contains(&*expected),
                "帮助里缺少「{expected}」：\n{rendered}"
            );
        }
        assert!(
            !rendered.contains("{usage}"),
            "模板占位符不该原样出现：\n{rendered}"
        );
    }
}
