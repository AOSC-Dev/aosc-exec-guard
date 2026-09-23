//! 询问用户：图形询问框（KDE 会话下用自带的 Kirigami 弹框，否则 kdialog /
//! zenity）、终端菜单（dialoguer），以及出错时的弹框。

use std::env;
use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use dialoguer::theme::ColorfulTheme;
use dialoguer::{Select, console::Term};
use rust_i18n::t;

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

/// 自带的 Kirigami 弹框（`data/dialog.qml`）用的退出码：0/1/2 是 qml 运行时自己的
/// 码（正常退出 / 加载出错…），所以避开不用。
const QML_RUN: i32 = 10;
const QML_DECLINE: i32 = 11;
const QML_RUN_REMEMBER: i32 = 12;

/// 图形弹框的候选工具。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DialogTool {
    /// 自带的 Kirigami QML 弹框（Qt6 的 qml 运行时 + `dialog.qml`）。
    Kirigami,
    /// KDE 的 kdialog（Qt Widgets，跟着 KDE 配色）。
    Kdialog,
    /// GNOME 那边的 zenity（GTK）。
    Zenity,
}

impl DialogTool {
    fn name(self) -> &'static str {
        match self {
            DialogTool::Kirigami => "qml",
            DialogTool::Kdialog => "kdialog",
            DialogTool::Zenity => "zenity",
        }
    }
}

/// 用哪些弹框、什么顺序：KDE 会话里自带的 Kirigami 框最顺眼，别的桌面上用它
/// 自己的工具（别把 KDE 味道的框摆到 GNOME 上）。
///
/// `AOSC_EXEC_GUARD_DIALOG` 可以钉死成某一个（`qml` / `kdialog` / `zenity`），
/// 测试和自定义用；取值不认识时按自动处理。
fn dialog_tools() -> Vec<DialogTool> {
    match env::var("AOSC_EXEC_GUARD_DIALOG").as_deref() {
        Ok("qml" | "kirigami") => vec![DialogTool::Kirigami],
        Ok("kdialog") => vec![DialogTool::Kdialog],
        Ok("zenity") => vec![DialogTool::Zenity],
        _ if kde_session() => vec![
            DialogTool::Kirigami,
            DialogTool::Kdialog,
            DialogTool::Zenity,
        ],
        _ => vec![DialogTool::Zenity, DialogTool::Kdialog],
    }
}

/// `--debug` 里显示弹框候选（逗号分隔）。
pub fn dialog_chain() -> String {
    dialog_tools()
        .iter()
        .map(|tool| tool.name())
        .collect::<Vec<_>>()
        .join(",")
}

/// 会话看起来是 KDE 吗（挑弹框用；看环境变量就够了，不猜别的）。
fn kde_session() -> bool {
    [
        "XDG_CURRENT_DESKTOP",
        "XDG_SESSION_DESKTOP",
        "DESKTOP_SESSION",
        "KDE_FULL_SESSION",
    ]
    .iter()
    .filter_map(env::var_os)
    .any(|value| {
        let value = value.to_string_lossy().to_ascii_lowercase();
        value.contains("kde") || value.contains("plasma")
    })
}

/// 弹框上要用的几句话（都来自 `locales/*.yml`）。
struct DialogText<'a> {
    title: &'a str,
    question: &'a str,
    run: &'a str,
    decline: &'a str,
    dont_ask_again: &'a str,
}

/// 图形询问：一个“不再询问”复选框加上运行/不运行两个按钮。
fn ask_dialog(entry: &QemuEntry, target: &Path, info: &crate::elf::ElfInfo) -> Option<AskOutcome> {
    let question = format!(
        "{}\n\n{}",
        qemu_question(target, info),
        t!("run-with-prompt", entry = entry.name)
    );
    let title = t!("dialog-title");
    let run_label = t!("run-label");
    let decline_label = t!("decline-label");
    let dont_ask_again = t!("dont-ask-again");
    let text = DialogText {
        title: &title,
        question: &question,
        run: &run_label,
        decline: &decline_label,
        dont_ask_again: &dont_ask_again,
    };

    for tool in dialog_tools() {
        let answer = match tool {
            DialogTool::Kirigami => ask_kirigami(&text),
            DialogTool::Kdialog => ask_kdialog(&text),
            DialogTool::Zenity => ask_zenity(&text),
        };
        if answer.is_some() {
            return answer;
        }
    }
    None
}

/// 自带的 Kirigami 弹框：Qt6 的 qml 运行时跑 `dialog.qml`。
///
/// 找不到运行时 / QML 文件，或者 qml 自己出错（退出码不是我们的约定）= 没答案，
/// 交给下一个候选。
fn ask_kirigami(text: &DialogText) -> Option<AskOutcome> {
    let runtime = qml_runtime()?;
    let file = qml_file();
    if !file.is_file() {
        return None;
    }
    let output = Command::new(runtime)
        .arg(&file)
        // qml 运行时的用法是 `qml [选项] <文件> [-- 参数…]`：只有 `--` 之后的
        // 参数才会原样传到 Qt.application.arguments（否则会被当成要加载的 QML
        // 文件）。
        .arg("--")
        .args([
            "--title",
            text.title,
            "--text",
            text.question,
            "--checkbox",
            text.dont_ask_again,
            "--ok",
            text.run,
            "--cancel",
            text.decline,
        ])
        .stdin(Stdio::null())
        .output()
        .ok()?;
    match output.status.code() {
        Some(QML_RUN) => Some(AskOutcome {
            run: true,
            remember: false,
        }),
        Some(QML_RUN_REMEMBER) => Some(AskOutcome {
            run: true,
            remember: true,
        }),
        Some(QML_DECLINE) => Some(AskOutcome {
            run: false,
            remember: false,
        }),
        _ => None,
    }
}

/// Qt6 的 QML 运行时。Qt5 的跑不了这个 QML（它要带版本的 import），所以用
/// `--version` 把候选筛一遍；`AOSC_EXEC_GUARD_QML` 可以指到别处（测试用）。
fn qml_runtime() -> Option<PathBuf> {
    if let Some(path) = env::var_os("AOSC_EXEC_GUARD_QML") {
        return Some(PathBuf::from(path));
    }
    ["/usr/lib/qt6/bin/qml", "/usr/bin/qml6", "/usr/bin/qml"]
        .iter()
        .map(PathBuf::from)
        .find(|path| {
            path.is_file()
                && Command::new(path)
                    .arg("--version")
                    .output()
                    .map(|out| String::from_utf8_lossy(&out.stdout).contains("Qml Runtime 6"))
                    .unwrap_or(false)
        })
}

/// 弹框 QML 的位置；`AOSC_EXEC_GUARD_QML_FILE` 可以指到别处（测试、或者自己
/// 改过的框）。
fn qml_file() -> PathBuf {
    env::var_os("AOSC_EXEC_GUARD_QML_FILE")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/usr/share/aosc-exec-guard/dialog.qml"))
}

/// zenity（GTK 那边）：问题对话框没有复选框，用只有一个条目的复选列表代替
/// （复选列表至少要有两列：第一列放复选框，第二列才是条目文字）。
fn ask_zenity(text: &DialogText) -> Option<AskOutcome> {
    let output = Command::new("zenity")
        .args([
            "--list",
            "--checklist",
            "--hide-header",
            &format!("--title={}", text.title),
            "--column= ",
            &format!("--column={}", text.dont_ask_again),
            "--print-column=2",
            &format!("--ok-label={}", text.run),
            &format!("--cancel-label={}", text.decline),
            &format!("--text={}", escape_markup(text.question)),
            "FALSE",
            text.dont_ask_again,
        ])
        .stdin(Stdio::null())
        .output()
        .ok()?;
    ask_outcome(&output)
}

/// kdialog：复选列表同理（它的消息框有复选框，但不能自定义按钮文字）。
fn ask_kdialog(text: &DialogText) -> Option<AskOutcome> {
    let output = Command::new("kdialog")
        .args([
            &format!("--title={}", text.title),
            &format!("--ok-label={}", text.run),
            &format!("--cancel-label={}", text.decline),
            "--checklist",
            text.question,
            "1",
            text.dont_ask_again,
            "off",
        ])
        .stdin(Stdio::null())
        .output()
        .ok()?;
    ask_outcome(&output)
}

/// 0 = 按了“运行”（勾选框时 zenity 打印条目文字、kdialog 打印条目编号），
/// 1 = 按了“不运行”，其它（没装、启动失败、被信号杀掉）= 本次没答案，换下一个工具试。
fn ask_outcome(output: &Output) -> Option<AskOutcome> {
    let printed = String::from_utf8_lossy(&output.stdout);
    match output.status.code() {
        Some(0) => Some(AskOutcome {
            run: true,
            remember: printed.contains('1') || printed.contains(&*t!("dont-ask-again")),
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

    // 以前是 [y/N/a/s]；“总是不运行”不再给菜单入口（要固定成从不运行就手写
    // `qemu = never` 配置或 `AOSC_EXEC_GUARD_QEMU=never`），默认项仍是“不运行”。
    let items = [t!("menu-run-once"), t!("menu-decline"), t!("menu-always")];

    // 菜单用 dialoguer 自带的 ColorfulTheme：黄 `?` + 灰 `›` 起头，绿色 `❯` 指着
    // 当前项（青色），确认后 `✔` 报告结果。默认主题是纯 ASCII，终端里太干。
    let theme = ColorfulTheme::default();
    let choice = Select::with_theme(&theme)
        .with_prompt(t!("run-with-prompt", entry = entry.name))
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
fn qemu_question(target: &Path, info: &crate::elf::ElfInfo) -> String {
    t!(
        "qemu-question",
        path = target.display().to_string(),
        sentence = arch_sentence(info, env::consts::ARCH)
    )
    .to_string()
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

        let _pin = crate::i18n::pin("zh-CN");
        // zenity 勾选时打印条目文字，kdialog 打印条目编号。“运行” = 0。
        let run = ask_outcome(&output(0, "不再询问\n")).unwrap();
        assert!(run.run && run.remember);
        let run = ask_outcome(&output(0, "1\n")).unwrap();
        assert!(run.run && run.remember);
        let run = ask_outcome(&output(0, "")).unwrap();
        assert!(run.run && !run.remember);
        // 英文界面下按本地化的勾选文字解析
        rust_i18n::set_locale("en");
        let run = ask_outcome(&output(0, "Don't ask again\n")).unwrap();
        assert!(run.run && run.remember);
        rust_i18n::set_locale("zh-CN");
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
