//! 询问用户：图形询问框（zenity/kdialog）、终端菜单（dialoguer），以及出错时
//! 的弹框。

use std::env;
use std::io::IsTerminal;
use std::path::Path;
use std::process::{Command, Output, Stdio};

use dialoguer::{Select, console::Term};

use crate::elf::arch_sentence;
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
    entry: &QemuEntry,
    target: &Path,
    info: &crate::elf::ElfInfo,
    mode: DisplayMode,
) -> Option<AskOutcome> {
    if mode == DisplayMode::Dialog
        && let Some(outcome) = ask_dialog(entry, target, info)
    {
        return Some(outcome);
    }
    ask_terminal(entry, target, info)
}

/// 图形询问：一个“不再询问”复选框加上运行/不运行两个按钮。
fn ask_dialog(entry: &QemuEntry, target: &Path, info: &crate::elf::ElfInfo) -> Option<AskOutcome> {
    let question = format!(
        "{}\n\n要用 {} 运行吗？",
        qemu_question(target, info),
        entry.name
    );

    // zenity 的问题对话框没有复选框，用只有一个条目的复选列表代替
    // （复选列表至少要有两列：第一列放复选框，第二列才是条目文字）。
    let zenity = Command::new("zenity")
        .args([
            "--list",
            "--checklist",
            "--hide-header",
            "--title=无法运行此程序",
            "--column= ",
            "--column=不再询问",
            "--print-column=2",
            "--ok-label=运行",
            "--cancel-label=不运行",
            &format!("--text={}", escape_markup(&question)),
            "FALSE",
            "不再询问",
        ])
        .stdin(Stdio::null())
        .output();
    if let Ok(output) = zenity
        && let Some(outcome) = ask_outcome(&output)
    {
        return Some(outcome);
    }

    // kdialog 的复选列表同理（它的消息框有复选框，但不能自定义按钮文字）。
    let kdialog = Command::new("kdialog")
        .args([
            "--title=无法运行此程序",
            "--ok-label=运行",
            "--cancel-label=不运行",
            "--checklist",
            &question,
            "1",
            "不再询问",
            "off",
        ])
        .stdin(Stdio::null())
        .output();
    if let Ok(output) = kdialog
        && let Some(outcome) = ask_outcome(&output)
    {
        return Some(outcome);
    }

    None
}

/// 0 = 按了“运行”（勾选框时 zenity 打印条目文字、kdialog 打印条目编号），
/// 1 = 按了“不运行”，其它（没装、启动失败、被信号杀掉）= 本次没答案，换下一个工具试。
fn ask_outcome(output: &Output) -> Option<AskOutcome> {
    let printed = String::from_utf8_lossy(&output.stdout);
    match output.status.code() {
        Some(0) => Some(AskOutcome {
            run: true,
            remember: printed.contains('1') || printed.contains("不再询问"),
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
    entry: &QemuEntry,
    target: &Path,
    info: &crate::elf::ElfInfo,
) -> Option<AskOutcome> {
    if !std::io::stdin().is_terminal() || !std::io::stderr().is_terminal() {
        return None;
    }

    eprintln!("aosc-exec-guard: {}", qemu_question(target, info));

    // 以前是 [y/N/a/s]；现在同样的四个答案摆成菜单，默认项仍是“不运行”（和 y/N 一致）。
    let items = [
        "运行（这次）",
        "不运行（这次）",
        "总是运行（不再询问）",
        "总是不运行（不再询问）",
    ];

    let choice = Select::new()
        .with_prompt(format!("用 {} 运行吗", entry.name))
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
        Ok(Some(3)) => AskOutcome {
            run: false,
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
fn qemu_question(target: &Path, info: &crate::elf::ElfInfo) -> String {
    format!(
        "“{}”不能在本机直接运行：{}。",
        target.display(),
        arch_sentence(info, env::consts::ARCH)
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
        let run = ask_outcome(&output(0, "不再询问\n")).unwrap();
        assert!(run.run && run.remember);
        let run = ask_outcome(&output(0, "1\n")).unwrap();
        assert!(run.run && run.remember);
        let run = ask_outcome(&output(0, "")).unwrap();
        assert!(run.run && !run.remember);
        // “不运行” = 1；启动失败/被信号杀死 = 没答案，换下一个对话框工具。
        let declined = ask_outcome(&output(1, "")).unwrap();
        assert!(!declined.run && !declined.remember);
        assert!(ask_outcome(&output(255, "")).is_none());
    }

    #[test]
    fn markup_is_escaped_for_zenity() {
        assert_eq!(escape_markup("a < b & c > d"), "a &lt; b &amp; c &gt; d");
    }
}
