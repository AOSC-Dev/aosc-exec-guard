//! ELF 解析与“能不能在本机跑”的判定。
//!
//! 只读文件头部的 20 字节，把结果归成 [`Verdict`]，并组织给用户看的解释文案
//! （[`build_message`]）。

use std::env;
use std::fs::File;
use std::io::Read;
use std::path::Path;

use crate::i18n::Lang;
use crate::platform::{in_chroot, in_container};
use crate::qemu::{QemuEntry, registry_visible};

pub const EI_CLASS: usize = 4;
pub const EI_DATA: usize = 5;
pub const E_MACHINE_OFF: usize = 18;
pub const ELFCLASS32: u8 = 1;
pub const ELFCLASS64: u8 = 2;
pub const ELFDATA2LSB: u8 = 1;
pub const ELFDATA2MSB: u8 = 2;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ElfClass {
    Bits32,
    Bits64,
}

impl ElfClass {
    pub fn bits(self) -> u32 {
        match self {
            ElfClass::Bits32 => 32,
            ElfClass::Bits64 => 64,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Endian {
    Little,
    Big,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ElfInfo {
    pub class: ElfClass,
    pub endian: Endian,
    pub machine: u16,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// The file targets a different machine than the host.
    ArchMismatch(ElfInfo),
    /// The file targets the host machine, but the kernel refused it anyway.
    NativeButRejected(ElfInfo),
    /// Not a (readable) ELF file.
    NotElf(NotElfReason),
}

/// 为什么“不是能跑的 ELF”（具体文案在 i18n 里）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NotElfReason {
    CannotRead,
    TooSmall,
    NotElf,
    BadClass,
    BadEndian,
    ReadFailed,
}

/// e_machine value of the machine we are running on, if we know it.
pub fn native_machine() -> Option<u16> {
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

pub fn machine_name(machine: u16) -> Option<&'static str> {
    Some(match machine {
        0x02 => "SPARC",
        0x03 => "i386（x86）",
        0x04 => "m68k",
        0x08 => "MIPS",
        0x12 => "SPARC32PLUS（SPARC V8+）",
        0x14 => "PowerPC",
        0x15 => "PowerPC 64",
        0x16 => "s390x",
        0x28 => "ARM",
        0x2a => "SuperH",
        0x2b => "SPARC64（SPARC v9）",
        0x3e => "x86_64",
        0xb7 => "aarch64（ARM64）",
        0xf3 => "RISC-V",
        0x102 => "LoongArch",
        0x9026 => "Alpha",
        0xbaab => "MicroBlaze",
        _ => return None,
    })
}

pub fn describe(lang: Lang, machine: u16) -> String {
    match machine_name(machine) {
        Some(name) => name.to_string(),
        None => lang.unknown_machine(machine),
    }
}

pub fn parse_elf(bytes: &[u8]) -> Result<ElfInfo, NotElfReason> {
    if bytes.len() < E_MACHINE_OFF + 2 {
        return Err(NotElfReason::TooSmall);
    }
    if &bytes[0..4] != b"\x7fELF" {
        return Err(NotElfReason::NotElf);
    }

    let class = match bytes[EI_CLASS] {
        ELFCLASS32 => ElfClass::Bits32,
        ELFCLASS64 => ElfClass::Bits64,
        _ => return Err(NotElfReason::BadClass),
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
        _ => return Err(NotElfReason::BadEndian),
    };

    Ok(ElfInfo {
        class,
        endian,
        machine,
    })
}

/// Read only the first 20 bytes; a full read is pointless for huge binaries.
pub fn classify(path: &Path, native: Option<u16>) -> Verdict {
    let mut file = match File::open(path) {
        Ok(file) => file,
        Err(_) => return Verdict::NotElf(NotElfReason::CannotRead),
    };
    let mut header = [0u8; E_MACHINE_OFF + 2];
    if let Err(err) = file.read_exact(&mut header) {
        return Verdict::NotElf(match err.kind() {
            std::io::ErrorKind::UnexpectedEof => NotElfReason::TooSmall,
            _ => NotElfReason::ReadFailed,
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
pub fn arch_sentence(lang: Lang, info: &ElfInfo, native_label: &str) -> String {
    lang.arch_sentence(
        &describe(lang, info.machine),
        info.class.bits(),
        native_label,
    )
}

pub fn build_message(
    lang: Lang,
    path: &Path,
    native_label: &str,
    verdict: &Verdict,
    qemu: Option<&QemuEntry>,
) -> String {
    let path = path.display().to_string();
    match verdict {
        Verdict::ArchMismatch(info) => {
            let hint = match qemu {
                Some(entry) => lang.hint_emulator_installed(&entry.name),
                // 宿主机注册的条目在 chroot 里照样会命中；但 guard 找模拟器时
                // 必须在脚下看到那个文件（F 只让内核重用注册时打开的解释器
                // 文件，guard 转发时 exec 的仍然是路径）。chroot（没挂 /proc，
                // 认不出来）、容器里都是这样：没有任何可达路径，只能解释。
                None if in_chroot() || in_container() || !registry_visible() => {
                    lang.hint_isolated().to_string()
                }
                None => lang.hint_install_emulator().to_string(),
            };
            lang.cannot_run(&path, &arch_sentence(lang, info, native_label), &hint)
        }
        Verdict::NativeButRejected(info) => {
            lang.native_but_rejected(&path, &describe(lang, info.machine))
        }
        Verdict::NotElf(reason) => lang.cannot_run_notelf(&path, lang.not_elf_reason(*reason)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

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
        let message = build_message(Lang::ZhCn, Path::new("/tmp/app"), "x86_64", &verdict, None);
        assert!(message.contains("aarch64"));
        assert!(message.contains("x86_64"));
        assert!(message.contains("64 位"));

        let english = build_message(Lang::En, Path::new("/tmp/app"), "x86_64", &verdict, None);
        assert!(english.contains("cannot run"));
        assert!(english.contains("64-bit"));
        assert!(!english.contains("无法"));
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
        let message = build_message(
            Lang::ZhCn,
            Path::new("/tmp/app"),
            "x86_64",
            &verdict,
            Some(&entry),
        );
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
        let message = build_message(Lang::ZhCn, Path::new("/tmp/app"), "x86_64", &verdict, None);
        assert!(message.contains("损坏"));
    }

    #[test]
    fn message_reports_non_elf() {
        let verdict = Verdict::NotElf(NotElfReason::NotElf);
        let message = build_message(Lang::ZhCn, Path::new("/tmp/app"), "x86_64", &verdict, None);
        assert!(message.contains("不是 ELF"));
    }
}
