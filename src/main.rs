//! aosc-exec-guard — PoC: explain why an executable refused to start.
//!
//! Registered as a `binfmt_misc` interpreter (see `data/aosc-exec-guard.conf`),
//! the kernel runs this program when the native `binfmt_elf` loader rejects a
//! file with `-ENOEXEC` — typically a foreign-architecture ELF binary.
//!
//! It parses the ELF header, prints a human-readable explanation to stderr and
//! optionally shows a dialog (zenity/kdialog) when it was launched from a
//! graphical session. When a matching qemu-user binfmt entry is installed it can
//! also offer to run the program through the emulator instead (see `--qemu`).
//! It exits with status 126, matching the shell convention for "cannot execute"
//! errors.

use std::env;
use std::ffi::OsString;
use std::fs::File;
use std::io::{IsTerminal, Read};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use clap::{Parser, ValueEnum};

/// Exit status for "found but cannot be executed" (shell convention).
const EXIT_CANNOT_EXEC: i32 = 126;

const EI_CLASS: usize = 4;
const EI_DATA: usize = 5;
const E_MACHINE_OFF: usize = 18;
const ELFCLASS32: u8 = 1;
const ELFCLASS64: u8 = 2;
const ELFDATA2LSB: u8 = 1;
const ELFDATA2MSB: u8 = 2;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ElfClass {
    Bits32,
    Bits64,
}

impl ElfClass {
    fn bits(self) -> u32 {
        match self {
            ElfClass::Bits32 => 32,
            ElfClass::Bits64 => 64,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Endian {
    Little,
    Big,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ElfInfo {
    class: ElfClass,
    endian: Endian,
    machine: u16,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Verdict {
    /// The file targets a different machine than the host.
    ArchMismatch(ElfInfo),
    /// The file targets the host machine, but the kernel refused it anyway.
    NativeButRejected(ElfInfo),
    /// Not a (readable) ELF file.
    NotElf(&'static str),
}

/// e_machine value of the machine we are running on, if we know it.
fn native_machine() -> Option<u16> {
    Some(match env::consts::ARCH {
        "x86" => 0x03,
        "x86_64" => 0x3e,
        "arm" => 0x28,
        "aarch64" => 0xb7,
        "riscv32" | "riscv64" => 0xf3,
        "loongarch64" => 0x102,
        "powerpc" => 0x14,
        "powerpc64" => 0x15,
        "s390x" => 0x16,
        "mips" | "mips64" => 0x08,
        _ => return None,
    })
}

fn machine_name(machine: u16) -> Option<&'static str> {
    Some(match machine {
        0x03 => "i386（x86）",
        0x08 => "MIPS",
        0x14 => "PowerPC",
        0x15 => "PowerPC 64",
        0x16 => "s390x",
        0x28 => "ARM",
        0x2a => "SuperH",
        0x3e => "x86_64",
        0xb7 => "aarch64（ARM64）",
        0xf3 => "RISC-V",
        0x102 => "LoongArch",
        _ => return None,
    })
}

fn describe(machine: u16) -> String {
    match machine_name(machine) {
        Some(name) => name.to_string(),
        None => format!("未知架构（e_machine=0x{machine:x}）"),
    }
}

fn parse_elf(bytes: &[u8]) -> Result<ElfInfo, &'static str> {
    if bytes.len() < E_MACHINE_OFF + 2 {
        return Err("文件太小，不是一个有效的 ELF 可执行文件");
    }
    if &bytes[0..4] != b"\x7fELF" {
        return Err("文件不是 ELF 可执行文件");
    }

    let class = match bytes[EI_CLASS] {
        ELFCLASS32 => ElfClass::Bits32,
        ELFCLASS64 => ElfClass::Bits64,
        _ => return Err("ELF 头部的位宽字段无法识别"),
    };

    let (endian, machine) = match bytes[EI_DATA] {
        ELFDATA2LSB => (
            Endian::Little,
            u16::from_le_bytes([bytes[E_MACHINE_OFF], bytes[E_MACHINE_OFF + 1]]),
        ),
        ELFDATA2MSB => (
            Endian::Big,
            u16::from_be_bytes([bytes[E_MACHINE_OFF], bytes[E_MACHINE_OFF + 1]]),
        ),
        _ => return Err("ELF 头部的字节序字段无法识别"),
    };

    Ok(ElfInfo {
        class,
        endian,
        machine,
    })
}

/// Read only the first 20 bytes; a full read is pointless for huge binaries.
fn classify(path: &Path, native: Option<u16>) -> Verdict {
    let mut file = match File::open(path) {
        Ok(file) => file,
        Err(_) => return Verdict::NotElf("无法读取该文件"),
    };
    let mut header = [0u8; E_MACHINE_OFF + 2];
    if let Err(err) = file.read_exact(&mut header) {
        return Verdict::NotElf(match err.kind() {
            std::io::ErrorKind::UnexpectedEof => "文件太小，不是一个有效的 ELF 可执行文件",
            _ => "读取文件失败",
        });
    }
    match parse_elf(&header) {
        Err(why) => Verdict::NotElf(why),
        Ok(info) => match native {
            Some(code) if code == info.machine => Verdict::NativeButRejected(info),
            _ => Verdict::ArchMismatch(info),
        },
    }
}

/// “该程序是为 X 构建的 N 位程序，而本机是 Y”。
fn arch_sentence(info: &ElfInfo, native_label: &str) -> String {
    format!(
        "该程序是为 {}构建的 {} 位程序，而本机是 {native_label}",
        describe(info.machine),
        info.class.bits()
    )
}

fn build_message(
    path: &Path,
    native_label: &str,
    verdict: &Verdict,
    qemu: Option<&QemuEntry>,
) -> String {
    let path = path.display();
    match verdict {
        Verdict::ArchMismatch(info) => {
            let hint = match qemu {
                Some(entry) => format!(
                    "提示：本机已安装 {}，设置 AOSC_EXEC_GUARD_QEMU=always 可让它自动运行\
                     （ask=每次询问、never=从不运行）。",
                    entry.name
                ),
                None => "提示：可以安装对应架构的模拟器（qemu-user-static、box64 等）后重试，\
                         或改用 AOSC OS 原生版本。"
                    .to_string(),
            };
            format!(
                "无法运行“{path}”：{}。\n{hint}",
                arch_sentence(info, native_label)
            )
        }
        Verdict::NativeButRejected(info) => format!(
            "无法运行“{path}”：程序架构与本机一致（{}），\
             但内核拒绝了它，文件可能已损坏或格式不受支持。",
            describe(info.machine),
        ),
        Verdict::NotElf(why) => format!("无法运行“{path}”：{why}。"),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DisplayMode {
    Dialog,
    Text,
}

fn gui_available() -> bool {
    let set = |key: &str| env::var_os(key).is_some_and(|v| !v.is_empty());
    set("DISPLAY") || set("WAYLAND_DISPLAY")
}

fn in_terminal() -> bool {
    std::io::stdout().is_terminal() || std::io::stderr().is_terminal()
}

/// systemd unit context: never block on an interactive dialog.
fn in_service() -> bool {
    env::var_os("INVOCATION_ID").is_some()
}

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

    /// 打印模式判定等调试信息（相当于 AOSC_EXEC_GUARD_DEBUG=1）
    #[arg(long)]
    debug: bool,

    /// 检测到 qemu-user 仿真器时怎么办（环境变量 AOSC_EXEC_GUARD_QEMU；默认读用户配置）
    #[arg(long, value_enum, value_name = "模式")]
    qemu: Option<QemuMode>,
}

/// 环境变量开关按「存在即开启」处理：内核调用时没法给 guard 传选项。
fn env_switch(key: &str) -> bool {
    env::var_os(key).is_some()
}

fn decide_mode(no_dialog: bool) -> DisplayMode {
    if no_dialog {
        return DisplayMode::Text;
    }

    if in_service() || !gui_available() || in_terminal() {
        return DisplayMode::Text;
    }

    DisplayMode::Dialog
}

/// 检测到匹配的 qemu-user 条目时怎么办。
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum QemuMode {
    /// 先询问（默认）
    Ask,
    /// 总是直接交给仿真器运行
    Always,
    /// 从不运行，只做解释
    Never,
}

fn parse_qemu_mode(value: &str) -> Option<QemuMode> {
    match value.trim() {
        "ask" => Some(QemuMode::Ask),
        "always" => Some(QemuMode::Always),
        "never" => Some(QemuMode::Never),
        _ => None,
    }
}

/// 优先级：命令行 > 环境变量 > 用户配置（“不再询问”记住的选择）。
fn resolve_qemu_mode(cli: &Cli) -> QemuMode {
    if let Some(mode) = cli.qemu {
        return mode;
    }
    if let Some(mode) = env::var_os("AOSC_EXEC_GUARD_QEMU")
        .and_then(|value| value.to_str().and_then(parse_qemu_mode))
    {
        return mode;
    }
    load_saved_qemu_mode().unwrap_or(QemuMode::Ask)
}

/// 用户配置（`~/.config/aosc-exec-guard.conf`）。
fn config_path() -> Option<PathBuf> {
    let base = match env::var_os("XDG_CONFIG_HOME").filter(|dir| !dir.is_empty()) {
        Some(dir) => PathBuf::from(dir),
        None => PathBuf::from(env::var_os("HOME")?).join(".config"),
    };
    Some(base.join("aosc-exec-guard.conf"))
}

fn load_saved_qemu_mode() -> Option<QemuMode> {
    let text = std::fs::read_to_string(config_path()?).ok()?;
    text.lines().find_map(|line| {
        let line = line.split('#').next().unwrap_or("").trim();
        let (key, value) = line.split_once('=')?;
        (key.trim() == "qemu")
            .then(|| parse_qemu_mode(value))
            .flatten()
    })
}

fn save_qemu_mode(mode: QemuMode) -> std::io::Result<()> {
    let path =
        config_path().ok_or_else(|| std::io::Error::other("HOME 未设置，无法定位用户配置"))?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let value = match mode {
        QemuMode::Ask => "ask",
        QemuMode::Always => "always",
        QemuMode::Never => "never",
    };
    std::fs::write(
        path,
        format!(
            "# aosc-exec-guard 用户设置（“不再询问”时写入）\n\
             # qemu: ask=每次询问（默认）/ always=总是用仿真器运行 / never=从不运行\n\
             qemu = {value}\n"
        ),
    )
}

/// `/proc/sys/fs/binfmt_misc`；测试可用 AOSC_EXEC_GUARD_BINFMT_DIR 指到假目录。
fn binfmt_dir() -> PathBuf {
    env::var_os("AOSC_EXEC_GUARD_BINFMT_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/proc/sys/fs/binfmt_misc"))
}

/// qemu-user 的 binfmt 条目名（与 qemu-binfmt-conf.sh 一致）。
fn qemu_entry_names(info: &ElfInfo) -> &'static [&'static str] {
    use ElfClass::{Bits32, Bits64};
    use Endian::{Big, Little};
    match (info.machine, info.class, info.endian) {
        (0x03, _, _) => &["qemu-i386"],
        (0x3e, _, _) => &["qemu-x86_64"],
        (0x28, _, Little) => &["qemu-arm"],
        (0x28, _, Big) => &["qemu-armeb"],
        (0xb7, _, Little) => &["qemu-aarch64"],
        (0xb7, _, Big) => &["qemu-aarch64_be"],
        (0xf3, Bits32, Little) => &["qemu-riscv32"],
        (0xf3, Bits64, Little) => &["qemu-riscv64"],
        (0x102, _, Little) => &["qemu-loongarch64"],
        (0x08, Bits32, Little) => &["qemu-mipsel"],
        (0x08, Bits32, Big) => &["qemu-mips"],
        (0x08, Bits64, Little) => &["qemu-mips64el"],
        (0x08, Bits64, Big) => &["qemu-mips64"],
        (0x14, _, _) => &["qemu-ppc"],
        (0x15, _, Little) => &["qemu-ppc64le"],
        (0x15, _, Big) => &["qemu-ppc64"],
        (0x16, _, _) => &["qemu-s390x"],
        (0x2a, _, Little) => &["qemu-sh4"],
        (0x2a, _, Big) => &["qemu-sh4eb"],
        (0x02, _, _) => &["qemu-sparc"],
        (0x2b, _, _) => &["qemu-sparc64"],
        _ => &[],
    }
}

#[derive(Debug, Clone)]
struct QemuEntry {
    /// binfmt 条目名，如 `qemu-aarch64`
    name: String,
    /// 条目里的解释器，如 `/usr/bin/qemu-aarch64-static`
    interpreter: PathBuf,
    /// 条目的 flags 字段（`F`、`OCF` 之类）
    flags: String,
}

/// 找一个已启用、且解释器还在的 qemu 条目。
fn find_qemu_entry(info: &ElfInfo) -> Option<QemuEntry> {
    qemu_entry_names(info).iter().find_map(|name| {
        let text = std::fs::read_to_string(binfmt_dir().join(name)).ok()?;
        let entry = parse_qemu_entry(name, &text)?;
        entry.interpreter.is_file().then_some(entry)
    })
}

/// 解析条目文件：第一行是启用状态，另有 `interpreter <路径>` 与 `flags:` 行。
fn parse_qemu_entry(name: &str, text: &str) -> Option<QemuEntry> {
    let mut lines = text.lines();
    if lines.next()?.trim() != "enabled" {
        return None;
    }
    let mut interpreter = None;
    let mut flags = String::new();
    for line in lines {
        if let Some(path) = line.strip_prefix("interpreter ") {
            interpreter = Some(PathBuf::from(path.trim()));
        } else if let Some(value) = line.strip_prefix("flags:") {
            flags = value.trim().to_string();
        }
    }
    Some(QemuEntry {
        name: name.to_string(),
        interpreter: interpreter?,
        flags,
    })
}

/// 询问结果。
struct AskOutcome {
    /// 用户选择运行。
    run: bool,
    /// 用户勾了“不再询问”，这个选择要记住。
    remember: bool,
}

/// 询问是否交给 qemu 运行；返回 None 表示没有可用的询问界面（不打扰用户）。
fn ask_run_via_qemu(
    entry: &QemuEntry,
    target: &Path,
    info: &ElfInfo,
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
fn ask_dialog(entry: &QemuEntry, target: &Path, info: &ElfInfo) -> Option<AskOutcome> {
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

/// 终端询问；stdin 不是终端时返回 None（不打扰用户）。
fn ask_terminal(entry: &QemuEntry, target: &Path, info: &ElfInfo) -> Option<AskOutcome> {
    if !std::io::stdin().is_terminal() {
        return None;
    }
    eprintln!("aosc-exec-guard: {}", qemu_question(target, info));
    eprintln!(
        "aosc-exec-guard: 用 {} 运行吗？[y/N]（a=总是运行，s=总是不运行）",
        entry.name
    );
    let mut answer = String::new();
    let _ = std::io::stdin().read_line(&mut answer);
    Some(match answer.trim() {
        "y" | "Y" => AskOutcome {
            run: true,
            remember: false,
        },
        "a" | "A" => AskOutcome {
            run: true,
            remember: true,
        },
        "s" | "S" => AskOutcome {
            run: false,
            remember: true,
        },
        _ => AskOutcome {
            run: false,
            remember: false,
        },
    })
}

/// 询问时的第一句：说明为什么本机不能直接跑。
fn qemu_question(target: &Path, info: &ElfInfo) -> String {
    format!(
        "“{}”不能在本机直接运行：{}。",
        target.display(),
        arch_sentence(info, env::consts::ARCH)
    )
}

/// 把控制权交给 qemu：按内核的 argv 布局调用解释器。只有启动失败才会返回。
fn run_via_qemu(entry: &QemuEntry, target: &Path, program_args: &[OsString]) -> std::io::Error {
    let mut command = Command::new(&entry.interpreter);
    command.arg(target);
    if entry.flags.contains('P') {
        // 条目带 P 时内核会多插一个原 argv[0]；guard 拿不到它，用程序路径顶替。
        command.arg(target);
    }
    command.args(program_args);
    command.exec()
}

/// Show a modal error dialog. Returns false when no dialog tool worked.
fn show_dialog(title: &str, body: &str) -> bool {
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

fn main() {
    let cli = Cli::parse();
    let (target, program_args) = cli.argv.split_first().expect("clap 保证至少有一个程序路径");
    let target = Path::new(target);

    let no_dialog = cli.no_dialog || env_switch("AOSC_EXEC_GUARD_NO_DIALOG");
    let debug = cli.debug || env_switch("AOSC_EXEC_GUARD_DEBUG");
    let qemu_mode = resolve_qemu_mode(&cli);

    let native = native_machine();
    let verdict = classify(target, native);
    let qemu_entry = match &verdict {
        Verdict::ArchMismatch(info) => find_qemu_entry(info),
        _ => None,
    };

    let mode = decide_mode(no_dialog);
    if debug {
        eprintln!(
            "[debug] mode={mode:?} qemu={} qemu_mode={qemu_mode:?} gui={} tty={} in_service={}",
            qemu_entry.as_ref().map_or("-", |entry| entry.name.as_str()),
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

#[cfg(test)]
mod tests {
    use super::*;

    fn header64(machine: u16, little_endian: bool) -> Vec<u8> {
        let mut header = vec![0u8; 64];
        header[0..4].copy_from_slice(b"\x7fELF");
        header[EI_CLASS] = ELFCLASS64;
        header[EI_DATA] = if little_endian {
            ELFDATA2LSB
        } else {
            ELFDATA2MSB
        };
        header[6] = 1;
        header[16] = 2; // e_type = ET_EXEC
        let bytes = if little_endian {
            machine.to_le_bytes()
        } else {
            machine.to_be_bytes()
        };
        header[E_MACHINE_OFF] = bytes[0];
        header[E_MACHINE_OFF + 1] = bytes[1];
        header
    }

    #[test]
    fn parses_aarch64_elf64() {
        let info = parse_elf(&header64(0xb7, true)).unwrap();
        assert_eq!(info.machine, 0xb7);
        assert_eq!(info.class, ElfClass::Bits64);
    }

    #[test]
    fn parses_i386_elf32() {
        let mut header = header64(0x03, true);
        header[EI_CLASS] = ELFCLASS32;
        let info = parse_elf(&header).unwrap();
        assert_eq!(info.machine, 0x03);
        assert_eq!(info.class, ElfClass::Bits32);
    }

    #[test]
    fn parses_big_endian_s390x() {
        let info = parse_elf(&header64(0x16, false)).unwrap();
        assert_eq!(info.machine, 0x16);
    }

    #[test]
    fn rejects_bad_magic_and_short_files() {
        assert!(parse_elf(b"not an elf file at all").is_err());
        assert!(parse_elf(b"\x7fEL").is_err());
    }

    #[test]
    fn message_reports_mismatch_between_arches() {
        let verdict = Verdict::ArchMismatch(ElfInfo {
            class: ElfClass::Bits64,
            endian: Endian::Little,
            machine: 0xb7,
        });
        let message = build_message(Path::new("/tmp/app"), "x86_64", &verdict, None);
        assert!(message.contains("aarch64"));
        assert!(message.contains("x86_64"));
        assert!(message.contains("64 位"));
    }

    #[test]
    fn message_mentions_installed_emulator() {
        let verdict = Verdict::ArchMismatch(ElfInfo {
            class: ElfClass::Bits64,
            endian: Endian::Little,
            machine: 0xb7,
        });
        let entry = QemuEntry {
            name: "qemu-aarch64".to_string(),
            interpreter: PathBuf::from("/usr/bin/qemu-aarch64-static"),
            flags: "OCF".to_string(),
        };
        let message = build_message(Path::new("/tmp/app"), "x86_64", &verdict, Some(&entry));
        assert!(message.contains("qemu-aarch64"));
        assert!(message.contains("AOSC_EXEC_GUARD_QEMU"));
    }

    #[test]
    fn message_reports_native_but_rejected() {
        let verdict = Verdict::NativeButRejected(ElfInfo {
            class: ElfClass::Bits64,
            endian: Endian::Little,
            machine: 0x3e,
        });
        let message = build_message(Path::new("/tmp/app"), "x86_64", &verdict, None);
        assert!(message.contains("损坏"));
    }

    #[test]
    fn message_reports_non_elf() {
        let verdict = Verdict::NotElf("文件不是 ELF 可执行文件");
        let message = build_message(Path::new("/tmp/app"), "x86_64", &verdict, None);
        assert!(message.contains("不是 ELF"));
    }

    #[test]
    fn qemu_entry_names_follow_arch_and_endianness() {
        let elf = |class, endian, machine| ElfInfo {
            class,
            endian,
            machine,
        };
        assert_eq!(
            qemu_entry_names(&elf(ElfClass::Bits64, Endian::Little, 0xb7)),
            ["qemu-aarch64"]
        );
        assert_eq!(
            qemu_entry_names(&elf(ElfClass::Bits32, Endian::Big, 0x28)),
            ["qemu-armeb"]
        );
        assert_eq!(
            qemu_entry_names(&elf(ElfClass::Bits64, Endian::Little, 0xf3)),
            ["qemu-riscv64"]
        );
        assert!(qemu_entry_names(&elf(ElfClass::Bits64, Endian::Little, 0x1234)).is_empty());
    }

    #[test]
    fn parses_a_qemu_binfmt_entry() {
        let text = "enabled\ninterpreter /usr/bin/qemu-aarch64-static\nflags: OCF\noffset 0\n";
        let entry = parse_qemu_entry("qemu-aarch64", text).unwrap();
        assert_eq!(entry.name, "qemu-aarch64");
        assert_eq!(entry.interpreter, Path::new("/usr/bin/qemu-aarch64-static"));
        assert!(entry.flags.contains('O'));

        assert!(parse_qemu_entry("qemu-aarch64", "disabled\ninterpreter /bin/true\n").is_none());
        assert!(parse_qemu_entry("qemu-aarch64", "enabled\nflags: F\n").is_none());
    }

    #[test]
    fn qemu_mode_parsing_and_precedence_strings() {
        assert_eq!(parse_qemu_mode("always"), Some(QemuMode::Always));
        assert_eq!(parse_qemu_mode(" never "), Some(QemuMode::Never));
        assert_eq!(parse_qemu_mode("ask"), Some(QemuMode::Ask));
        assert_eq!(parse_qemu_mode("sometimes"), None);
    }

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
