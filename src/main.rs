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
//! `prompt`（询问界面）、`platform`（环境判定）、`config`（设置）；这里只留
//! 命令行解析和主流程。

mod config;
mod elf;
mod platform;
mod prompt;
mod qemu;

use std::env;
use std::ffi::OsString;
use std::path::Path;

use clap::Parser;

use crate::config::{QemuMode, resolve_qemu_mode, save_qemu_mode};
use crate::elf::{Verdict, build_message, classify, native_machine};
use crate::platform::{
    DisplayMode, decide_mode, env_switch, gui_available, in_service, in_terminal,
};
use crate::prompt::{ask_run_via_qemu, show_dialog};
use crate::qemu::{find_qemu_entry, registry_visible, run_via_qemu};

/// Exit status for "found but cannot be executed" (shell convention).
const EXIT_CANNOT_EXEC: i32 = 126;

/// 说明一个程序为何无法在本机运行（binfmt_misc 解释器）。
///
/// 内核通过 binfmt_misc 调用时为
/// `aosc-exec-guard <程序路径> <原程序的参数…>`：路径之后的内容一律属于
/// 原程序，guard 只是原样接受、不当成自己的选项解析。
#[derive(Debug, Parser)]
#[command(
    version,
    override_usage = "aosc-exec-guard [选项] <程序路径> [参数…]",
    after_help = "（通常由内核通过 binfmt_misc 调用，无需手动运行。）"
)]
struct Cli {
    /// 无法运行的程序路径（其后的参数原属于原程序）
    #[arg(
        value_name = "程序路径",
        required = true,
        trailing_var_arg = true,
        allow_hyphen_values = true
    )]
    argv: Vec<OsString>,

    /// 永远不弹框（相当于 AOSC_EXEC_GUARD_NO_DIALOG=1）
    #[arg(long)]
    no_dialog: bool,

    /// 打印模式判定等调试信息（含 binfmt 注册表可见性；相当于 AOSC_EXEC_GUARD_DEBUG=1）
    #[arg(long)]
    debug: bool,

    /// 检测到 qemu-user 仿真器时怎么办（环境变量 AOSC_EXEC_GUARD_QEMU；默认读用户配置）
    #[arg(long, value_enum, value_name = "模式")]
    qemu: Option<QemuMode>,
}

fn main() {
    let cli = Cli::parse();
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
            "[debug] mode={mode:?} qemu={} qemu_mode={qemu_mode:?} registry={} gui={} tty={} in_service={}",
            qemu_entry.as_ref().map_or("-", |entry| entry.name.as_str()),
            if registry_visible() {
                "visible"
            } else {
                "invisible"
            },
            gui_available(),
            in_terminal(),
            in_service(),
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
                            eprintln!("aosc-exec-guard: 无法记住“不再询问”的选择：{err}");
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
            eprintln!("aosc-exec-guard: 无法启动 {}：{err}", entry.name);
        }
    }

    let message = build_message(target, env::consts::ARCH, &verdict, qemu_entry.as_ref());
    eprintln!("aosc-exec-guard: {message}");
    if mode == DisplayMode::Dialog {
        let _ = show_dialog("无法运行此程序", &message);
    }
    std::process::exit(EXIT_CANNOT_EXEC);
}
