//! aosc-exec-guard — PoC: explain why an executable refused to start.
//!
//! Registered as a `binfmt_misc` interpreter (see `data/aosc-exec-guard.conf`),
//! the kernel runs this program when the native `binfmt_elf` loader rejects a
//! file with `-ENOEXEC` — typically a foreign-architecture ELF binary.
//!
//! It parses the ELF header, prints a human-readable explanation to stderr and
//! optionally shows a dialog (zenity/kdialog) when it was launched from a
//! graphical session. It exits with status 126, matching the shell convention
//! for "cannot execute" errors.

use std::env;
use std::ffi::OsString;
use std::fs::File;
use std::io::{IsTerminal, Read};
use std::path::Path;
use std::process::{Command, Stdio};

use clap::Parser;

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
struct ElfInfo {
    class: ElfClass,
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

    let machine = match bytes[EI_DATA] {
        ELFDATA2LSB => u16::from_le_bytes([bytes[E_MACHINE_OFF], bytes[E_MACHINE_OFF + 1]]),
        ELFDATA2MSB => u16::from_be_bytes([bytes[E_MACHINE_OFF], bytes[E_MACHINE_OFF + 1]]),
        _ => return Err("ELF 头部的字节序字段无法识别"),
    };

    Ok(ElfInfo { class, machine })
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

fn build_message(path: &Path, native_label: &str, verdict: &Verdict) -> String {
    let path = path.display();
    match verdict {
        Verdict::ArchMismatch(info) => format!(
            "无法运行“{path}”：该程序是为 {}构建的 {} 位程序，而本机是 {native_label}。\n\
             提示：可以安装对应架构的模拟器（qemu-user-static、box64 等）后重试，\
             或改用 AOSC OS 原生版本。",
            describe(info.machine),
            info.class.bits(),
        ),
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
    let (target, _program_args) = cli.argv.split_first().expect("clap 保证至少有一个程序路径");
    let target = Path::new(target);

    let no_dialog = cli.no_dialog || env_switch("AOSC_EXEC_GUARD_NO_DIALOG");
    let debug = cli.debug || env_switch("AOSC_EXEC_GUARD_DEBUG");

    let native = native_machine();
    let verdict = classify(target, native);
    let message = build_message(target, env::consts::ARCH, &verdict);
    eprintln!("aosc-exec-guard: {message}");

    let mode = decide_mode(no_dialog);
    if debug {
        eprintln!(
            "[debug] mode={mode:?} gui={} tty={} in_service={}",
            gui_available(),
            in_terminal(),
            in_service(),
        );
    }
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
        header[EI_DATA] = if little_endian { ELFDATA2LSB } else { ELFDATA2MSB };
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
            machine: 0xb7,
        });
        let message = build_message(Path::new("/tmp/app"), "x86_64", &verdict);
        assert!(message.contains("aarch64"));
        assert!(message.contains("x86_64"));
        assert!(message.contains("64 位"));
    }

    #[test]
    fn message_reports_native_but_rejected() {
        let verdict = Verdict::NativeButRejected(ElfInfo {
            class: ElfClass::Bits64,
            machine: 0x3e,
        });
        let message = build_message(Path::new("/tmp/app"), "x86_64", &verdict);
        assert!(message.contains("损坏"));
    }

    #[test]
    fn message_reports_non_elf() {
        let verdict = Verdict::NotElf("文件不是 ELF 可执行文件");
        let message = build_message(Path::new("/tmp/app"), "x86_64", &verdict);
        assert!(message.contains("不是 ELF"));
    }

    #[test]
    fn markup_is_escaped_for_zenity() {
        assert_eq!(escape_markup("a < b & c > d"), "a &lt; b &amp; c &gt; d");
    }
}
