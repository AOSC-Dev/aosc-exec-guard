//! 询问用户：图形询问框（zenity/kdialog）、终端菜单（dialoguer），以及出错时
//! 的弹框。

use std::env;
use std::io::IsTerminal;
use std::path::Path;
use std::process::{Command, Output, Stdio};

use dialoguer::{Select, console::Term};

use crate::elf::arch_sentence;
use crate::i18n::Lang;
use crate::platform::DisplayMode;
use crate::qemu::QemuEntry;

/// 询问结果。
pub struct AskOutcome {
    /// 用户选择运行。
    pub run: bool,
    /// 用户勾了“不再询问”，这个选择要记住。
    pub remember: bool,
}

/// 询问是否交给 qemu 运行；返回 None 表示没有可用的询问界面（不打扰用户）。
pub fn ask_run_via_qemu(
    lang: Lang,
    entry: &QemuEntry,
    target: &Path,
    info: &crate::elf::ElfInfo,
    mode: DisplayMode,
) -> Option<AskOutcome> {
    if mode == DisplayMode::Dialog
        && let Some(outcome) = ask_dialog(lang, entry, target, info)
    {
        return Some(outcome);
    }
    ask_terminal(lang, entry, target, info)
}

/// 图形询问：一个“不再询问”复选框加上运行/不运行两个按钮。
fn ask_dialog(
    lang: Lang,
    entry: &QemuEntry,
    target: &Path,
    info: &crate::elf::ElfInfo,
) -> Option<AskOutcome> {
    let question = format!(
        "{}\n\n{}",
        qemu_question(lang, target, info),
        lang.run_with_prompt(&entry.name)
    );

    // zenity 的问题对话框没有复选框，用只有一个条目的复选列表代替
    // （复选列表至少要有两列：第一列放复选框，第二列才是条目文字）。
    let zenity = Command::new("zenity")
        .args([
            "--list",
            "--checklist",
            "--hide-header",
            &format!("--title={}", lang.dialog_title()),
            "--column= ",
            &format!("--column={}", lang.dont_ask_again()),
            "--print-column=2",
            &format!("--ok-label={}", lang.run_label()),
            &format!("--cancel-label={}", lang.decline_label()),
            &format!("--text={}", escape_markup(&question)),
            "FALSE",
            lang.dont_ask_again(),
        ])
        .stdin(Stdio::null())
        .output();
    if let Ok(output) = zenity
        && let Some(outcome) = ask_outcome(&output, lang)
    {
        return Some(outcome);
    }

    // kdialog 的复选列表同理（它的消息框有复选框，但不能自定义按钮文字）。
    let kdialog = Command::new("kdialog")
        .args([
            &format!("--title={}", lang.dialog_title()),
            &format!("--ok-label={}", lang.run_label()),
            &format!("--cancel-label={}", lang.decline_label()),
            "--checklist",
            &question,
            "1",
            lang.dont_ask_again(),
            "off",
        ])
        .stdin(Stdio::null())
        .output();
    if let Ok(output) = kdialog
        && let Some(outcome) = ask_outcome(&output, lang)
    {
        return Some(outcome);
    }

    None
}

/// 0 = 按了“运行”（勾选框时 zenity 打印条目文字、kdialog 打印条目编号），
/// 1 = 按了“不运行”，其它（没装、启动失败、被信号杀掉）= 本次没答案，换下一个工具试。
fn ask_outcome(output: &Output, lang: Lang) -> Option<AskOutcome> {
    let printed = String::from_utf8_lossy(&output.stdout);
    match output.status.code() {
        Some(0) => Some(AskOutcome {
            run: true,
            remember: printed.contains('1') || printed.contains(lang.dont_ask_again()),
        }),
        Some(1) => Some(AskOutcome {
            run: false,
            remember: false,
        }),
        _ => None,
    }
}

/// 终端询问：用 dialoguer 画一个上下键选择的菜单。
/// stdin 读不了键、或 stderr 画不出菜单时返回 None（不打扰用户）。
fn ask_terminal(
    lang: Lang,
    entry: &QemuEntry,
    target: &Path,
    info: &crate::elf::ElfInfo,
) -> Option<AskOutcome> {
    if !std::io::stdin().is_terminal() || !std::io::stderr().is_terminal() {
        return None;
    }

    eprintln!("aosc-exec-guard: {}", qemu_question(lang, target, info));

    // 以前是 [y/N/a/s]；“总是不运行”不再给菜单入口（要固定成从不运行就手写
    // `qemu = never` 配置或 `AOSC_EXEC_GUARD_QEMU=never`），默认项仍是“不运行”。
    let items = [
        lang.menu_run_once(),
        lang.menu_decline(),
        lang.menu_always(),
    ];

    let choice = Select::new()
        .with_prompt(lang.run_with_prompt(&entry.name))
        .items(items)
        .default(1)
        .report(true)
        .interact_on_opt(&Term::stderr());

    Some(match choice {
        Ok(Some(0)) => AskOutcome {
            run: true,
            remember: false,
        },
        Ok(Some(2)) => AskOutcome {
            run: true,
            remember: true,
        },
        // 1 = “不运行（这次）”；q/Esc 退出、读键失败也都当这次不运行。
        _ => AskOutcome {
            run: false,
            remember: false,
        },
    })
}

/// 询问时的第一句：说明为什么本机不能直接跑。
fn qemu_question(lang: Lang, target: &Path, info: &crate::elf::ElfInfo) -> String {
    lang.qemu_question(
        &target.display().to_string(),
        &arch_sentence(lang, info, env::consts::ARCH),
    )
}

/// Show a modal error dialog. Returns false when no dialog tool worked.
pub fn show_dialog(title: &str, body: &str) -> bool {
    let markup = escape_markup(body);
    let attempts: [(&str, Vec<String>); 2] = [
        (
            "zenity",
            vec![
                "--error".to_string(),
                "--no-wrap".to_string(),
                format!("--title={title}"),
                format!("--text={markup}"),
            ],
        ),
        (
            "kdialog",
            vec![
                "--title".to_string(),
                title.to_string(),
                "--error".to_string(),
                body.to_string(),
            ],
        ),
    ];

    for (program, args) in attempts {
        let status = Command::new(program)
            .args(&args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        if status.is_ok() {
            return true;
        }
        // Command not found or failed to spawn: try the next one.
    }
    false
}

fn escape_markup(input: &str) -> String {
    input
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dialog_answers_are_parsed_from_exit_code_and_output() {
        use std::os::unix::process::ExitStatusExt;
        let output = |code: i32, stdout: &str| Output {
            status: std::process::ExitStatus::from_raw(code << 8),
            stdout: stdout.as_bytes().to_vec(),
            stderr: Vec::new(),
        };

        // zenity 勾选时打印条目文字，kdialog 打印条目编号。“运行” = 0。
        let run = ask_outcome(&output(0, "不再询问\n"), Lang::ZhCn).unwrap();
        assert!(run.run && run.remember);
        let run = ask_outcome(&output(0, "1\n"), Lang::ZhCn).unwrap();
        assert!(run.run && run.remember);
        let run = ask_outcome(&output(0, ""), Lang::ZhCn).unwrap();
        assert!(run.run && !run.remember);
        // 英文界面下按本地化的勾选文字解析
        let run = ask_outcome(&output(0, "Don't ask again\n"), Lang::En).unwrap();
        assert!(run.run && run.remember);
        // “不运行” = 1；启动失败/被信号杀死 = 没答案，换下一个对话框工具。
        let declined = ask_outcome(&output(1, ""), Lang::ZhCn).unwrap();
        assert!(!declined.run && !declined.remember);
        assert!(ask_outcome(&output(255, ""), Lang::ZhCn).is_none());
    }

    #[test]
    fn markup_is_escaped_for_zenity() {
        assert_eq!(escape_markup("a < b & c > d"), "a &lt; b &amp; c &gt; d");
    }
}
