//! 模拟器（qemu-user）的发现与转发。
//!
//! 只认 binfmt_misc 注册表：先看本进程可见的那份；看不到时（chroot / 容器里
//! 很常见）退回宿主机的注册表（`/proc/1/root`）。不猜 `/usr/bin/qemu-*` 之类
//! 的路径——两边都说没有就返回 None，由调用方解释退出。

use std::env;
use std::ffi::OsString;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::elf::{ElfClass, ElfInfo, Endian};

/// `/proc/sys/fs/binfmt_misc`；测试可用 AOSC_EXEC_GUARD_BINFMT_DIR 指到假目录。
pub fn binfmt_dir() -> PathBuf {
    env::var_os("AOSC_EXEC_GUARD_BINFMT_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/proc/sys/fs/binfmt_misc"))
}

/// qemu-user 的 binfmt 条目名（与 data/binfmt.d/ 里的规则一一对应，测试会校验）。
pub fn qemu_entry_names(info: &ElfInfo) -> &'static [&'static str] {
    use ElfClass::{Bits32, Bits64};
    use Endian::{Big, Little};
    match (info.machine, info.class, info.endian) {
        (0x02, Bits32, Big) => &["qemu-sparc"],
        (0x03, _, _) => &["qemu-i386"],
        (0x04, Bits32, Big) => &["qemu-m68k"],
        (0x08, Bits32, Little) => &["qemu-mipsel"],
        (0x08, Bits32, Big) => &["qemu-mips"],
        (0x08, Bits64, Little) => &["qemu-mips64el"],
        (0x08, Bits64, Big) => &["qemu-mips64"],
        (0x12, Bits32, Big) => &["qemu-sparc32plus"],
        (0x14, _, _) => &["qemu-ppc"],
        (0x15, _, Little) => &["qemu-ppc64le"],
        (0x15, _, Big) => &["qemu-ppc64"],
        (0x16, _, _) => &["qemu-s390x"],
        (0x28, _, Little) => &["qemu-arm"],
        (0x28, _, Big) => &["qemu-armeb"],
        (0x2a, _, Little) => &["qemu-sh4"],
        (0x2a, _, Big) => &["qemu-sh4eb"],
        (0x2b, Bits64, Big) => &["qemu-sparc64"],
        (0x3e, _, _) => &["qemu-x86_64"],
        (0xb7, _, Little) => &["qemu-aarch64"],
        (0xb7, _, Big) => &["qemu-aarch64_be"],
        (0xf3, Bits32, Little) => &["qemu-riscv32"],
        (0xf3, Bits64, Little) => &["qemu-riscv64"],
        (0x102, _, Little) => &["qemu-loongarch64"],
        (0x9026, Bits64, Little) => &["qemu-alpha"],
        (0xbaab, Bits32, Big) => &["qemu-microblaze"],
        _ => &[],
    }
}

#[derive(Debug, Clone)]
pub struct QemuEntry {
    /// binfmt 条目名，如 `qemu-aarch64`
    pub name: String,
    /// 条目里的解释器，如 `/usr/bin/qemu-aarch64-static`
    pub interpreter: PathBuf,
    /// 条目的 flags 字段（`F`、`OCF` 之类）
    pub flags: String,
}

/// 找一个已启用、且解释器还在的 qemu 条目。
///
/// 只认注册表：本进程看得见就用本地那份；看不见（chroot / 容器里很常见）就
/// 借宿主机的那份。两边都没有就返回 None——不做路径猜测。
pub fn find_qemu_entry(info: &ElfInfo) -> Option<QemuEntry> {
    if registry_visible() {
        return find_qemu_entry_in(&binfmt_dir(), info);
    }
    find_host_qemu_entry(info)
}

/// 本进程能看到 binfmt_misc 注册表吗——挂载点里有 `register` 文件才算数。
///
/// 看不到就意味着“不知道内核会怎么处理这个文件”（chroot / 容器里没挂、或者
/// 宿主就没挂），调用方据此决定是否要询问用户（见 config::resolve_qemu_mode）。
pub fn registry_visible() -> bool {
    let dir = binfmt_dir();
    // 测试钩子：指定了目录就以它为准（指到不存在的目录 = 看不到注册表）。
    // 注意：没挂 binfmt_misc 时，procfs 里也会有 `/proc/sys/fs/binfmt_misc` 这个
    // 空目录（chroot 里挂了 /proc 就是这样），所以默认看的是 `register` 文件。
    match env::var_os("AOSC_EXEC_GUARD_BINFMT_DIR") {
        Some(_) => dir.exists(),
        None => dir.join("register").exists(),
    }
}

fn find_qemu_entry_in(dir: &Path, info: &ElfInfo) -> Option<QemuEntry> {
    qemu_entry_names(info).iter().find_map(|name| {
        let text = std::fs::read_to_string(dir.join(name)).ok()?;
        let entry = parse_qemu_entry(name, &text)?;
        entry.interpreter.is_file().then_some(entry)
    })
}

/// 宿主机注册表（经由 `/proc/1/root`）：只在**共享 PID namespace** 时才真的是
/// “宿主机”的（普通 chroot 是；nspawn 这类容器里 /proc/1 是容器自己的 init，
/// 借不到宿主条目）。chroot 里条目带 `F` 时，内核执行的就是宿主机那份解释器
/// 文件，照着它转发最忠实。读不到（没挂 /proc、权限不够）就返回 None。
fn find_host_qemu_entry(info: &ElfInfo) -> Option<QemuEntry> {
    let base = Path::new("/proc/1/root");
    let dir = base.join("proc/sys/fs/binfmt_misc");
    qemu_entry_names(info).iter().find_map(|name| {
        let text = std::fs::read_to_string(dir.join(name)).ok()?;
        let mut entry = parse_qemu_entry(name, &text)?;
        // 在 chroot 里得经由 /proc/1/root 才能执行到宿主机那份。
        entry.interpreter = base.join(entry.interpreter.strip_prefix("/").ok()?);
        entry.interpreter.is_file().then_some(entry)
    })
}

/// 解析条目文件：第一行是启用状态，另有 `interpreter <路径>` 与 `flags:` 行。
pub fn parse_qemu_entry(name: &str, text: &str) -> Option<QemuEntry> {
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

/// 把控制权交给 qemu：按内核的 argv 布局调用解释器。只有启动失败才会返回。
pub fn run_via_qemu(entry: &QemuEntry, target: &Path, program_args: &[OsString]) -> std::io::Error {
    let mut command = Command::new(&entry.interpreter);
    command.arg(target);
    if entry.flags.contains('P') {
        // 条目带 P 时内核会多插一个原 argv[0]；guard 拿不到它，用程序路径顶替。
        command.arg(target);
    }
    command.args(program_args);
    command.exec()
}

#[cfg(test)]
mod tests {
    use super::*;

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

    /// 把 conf 里的 `\x7fELF…` 转回字节。
    fn parse_magic_bytes(field: &str) -> Vec<u8> {
        let mut bytes = Vec::new();
        let mut chars = field.chars().peekable();
        while let Some(ch) = chars.next() {
            if ch == '\\' && chars.peek() == Some(&'x') {
                chars.next();
                let hex: String = chars.by_ref().take(2).collect();
                bytes.push(u8::from_str_radix(&hex, 16).unwrap());
            } else {
                bytes.push(ch as u8);
            }
        }
        bytes
    }

    #[test]
    fn every_conf_rule_matches_the_arch_table() {
        use crate::elf::{E_MACHINE_OFF, EI_CLASS, EI_DATA, ELFCLASS32, ELFDATA2MSB};

        // 规则是逐字节从 AOSC 的 qemu-user 包抄来的，必须和 qemu_entry_names()
        // 一一对应，否则“发现可用的仿真器”这条路径会失效。
        let conf = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/data/binfmt.d/zz-aosc-exec-guard.conf.in"
        );
        let text =
            std::fs::read_to_string(conf).expect("读 data/binfmt.d/zz-aosc-exec-guard.conf.in");

        let mut rules = 0;
        for line in text.lines().map(str::trim) {
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            // :条目名:M::magic:mask:解释器:flags
            let fields: Vec<&str> = line.split(':').collect();
            let name = fields[1];
            let arch = name.strip_prefix("aosc-exec-guard-").expect("条目名");
            assert_eq!(fields[6], "/usr/bin/aosc-exec-guard", "{name} 的解释器");
            assert!(fields[7].contains('F'), "{name} 应带 F 标志");

            let magic = parse_magic_bytes(fields[4]);
            assert_eq!(&magic[0..4], b"\x7fELF", "{name} 的 magic");
            let (endian, machine) = match magic[EI_DATA] {
                ELFDATA2MSB => (
                    Endian::Big,
                    u16::from_be_bytes([magic[E_MACHINE_OFF], magic[E_MACHINE_OFF + 1]]),
                ),
                _ => (
                    Endian::Little,
                    u16::from_le_bytes([magic[E_MACHINE_OFF], magic[E_MACHINE_OFF + 1]]),
                ),
            };
            let info = ElfInfo {
                class: match magic[EI_CLASS] {
                    ELFCLASS32 => ElfClass::Bits32,
                    _ => ElfClass::Bits64,
                },
                endian,
                machine,
            };

            let expected = format!("qemu-{arch}");
            let names = qemu_entry_names(&info);
            assert!(
                names.contains(&expected.as_str()),
                "{name}（{info:?}）应映射到 {expected}，实际是 {names:?}"
            );
            rules += 1;
        }
        assert!(
            rules >= 20,
            "只解析到 {rules} 条规则，conf 是不是被截断了？"
        );
    }
}
