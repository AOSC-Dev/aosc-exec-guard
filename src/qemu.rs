//! 模拟器（qemu-user）的发现与转发。
//!
//! 认三处（按顺序）：本进程可见的 binfmt_misc 注册表；看不到时（chroot / 容器
//! 里很常见）退回宿主机的注册表（`/proc/1/root`）；都没有就翻
//! `/usr/lib/binfmt.alternatives/` 的候选 conf——box64 式打包（见
//! `scripts/install.sh --alternatives`）下，`/usr/lib/binfmt.d/emu-<arch>.conf`
//! 这个槽位指向 guard 自己的 conf，真正干活的模拟器就不在注册表里了。
//! 不猜 `/usr/bin/qemu-*` 之类的固定路径。

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

/// 找一个已启用、且解释器还在的模拟器条目。
///
/// 先查注册表（本进程可见的那份；看不见就借宿主机那份），没有再去
/// `/usr/lib/binfmt.alternatives/` 的候选 conf 里找（槽位被 guard 占住时
/// 模拟器只在那里）；三处都没有就返回 None——不做路径猜测。
pub fn find_qemu_entry(info: &ElfInfo) -> Option<QemuEntry> {
    let from_registry = if registry_visible() {
        find_qemu_entry_in(&binfmt_dir(), info)
    } else {
        find_host_qemu_entry(info)
    };
    from_registry.or_else(|| find_alternatives_entry(&alternatives_dir(), info))
}

/// alternatives 的候选目录（box64 式打包把 conf 放这儿，由 update-alternatives
/// 链接进 `/usr/lib/binfmt.d/emu-<arch>.conf`）。测试可用
/// AOSC_EXEC_GUARD_ALTERNATIVES_DIR 指到假目录。
pub fn alternatives_dir() -> PathBuf {
    env::var_os("AOSC_EXEC_GUARD_ALTERNATIVES_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/usr/lib/binfmt.alternatives"))
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

/// 注册表里没有时，翻 alternatives 的候选 conf：槽位（`emu-<arch>.conf`）被
/// guard 自己占住时，负责干活的模拟器（qemu / box64 / FEX…）就只剩这里能看。
/// 挑法：先按老规矩找名字是 `qemu-<arch>` 的那条（和以前注册表里会命中的条目
/// 一致），再退到第一条能命中这个 ELF 的候选（按 conf 文件名排序，结果确定）。
fn find_alternatives_entry(dir: &Path, info: &ElfInfo) -> Option<QemuEntry> {
    let mut files: Vec<PathBuf> = std::fs::read_dir(dir)
        .ok()?
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "conf"))
        .collect();
    files.sort();
    let mut candidates: Vec<QemuEntry> = Vec::new();
    for file in files {
        let Ok(text) = std::fs::read_to_string(&file) else {
            continue;
        };
        for line in text.lines() {
            if let Some(entry) = parse_conf_line(line, info)
                && entry.interpreter.is_file()
            {
                candidates.push(entry);
            }
        }
    }
    let names = qemu_entry_names(info);
    candidates
        .iter()
        .find(|entry| names.contains(&entry.name.as_str()))
        .or_else(|| candidates.first())
        .cloned()
}

/// 解析一行 binfmt conf（`:名字:M::magic:mask:解释器:flags`），只保留会命中
/// `info` 描述的这个 ELF 的（guard 自己的条目跳过）。
fn parse_conf_line(line: &str, info: &ElfInfo) -> Option<QemuEntry> {
    let line = line.trim();
    if !line.starts_with(':') || line.starts_with("#") {
        return None;
    }
    let mut fields = line.splitn(8, ':');
    fields.next()?; // 开头的空字段（行首的 ':'）
    let name = fields.next()?;
    if name.is_empty() || name.starts_with("aosc-exec-guard-") {
        return None;
    }
    if fields.next()? != "M" {
        return None; // 只认 magic 匹配的条目
    }
    let offset = fields.next()?;
    if !offset.is_empty() && offset != "0" {
        return None; // 偏移固定是 0（qemu / box64 的 conf 都写成空的 offset）
    }
    let magic = unescape_bytes(fields.next()?)?;
    let mask = unescape_bytes(fields.next()?)?;
    let interpreter = fields.next()?;
    let flags = fields.next().unwrap_or_default().to_string();
    if !magic_matches(&magic, &mask, info) {
        return None;
    }
    Some(QemuEntry {
        name: name.to_string(),
        interpreter: PathBuf::from(interpreter),
        flags,
    })
}

/// `\x7fELF` 这种写法 → 字节（conf 里的 magic/mask 只有 `\xNN` 和字面字符）。
fn unescape_bytes(field: &str) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    let mut chars = field.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            if chars.next()? != 'x' {
                return None;
            }
            let hi = chars.next()?.to_digit(16)?;
            let lo = chars.next()?.to_digit(16)?;
            out.push((hi * 16 + lo) as u8);
        } else if c.is_ascii() {
            out.push(c as u8);
        } else {
            return None;
        }
    }
    Some(out)
}

/// conf 的 magic/mask 会命中 `info` 这个 ELF 吗。
///
/// 只核对 conf 真正钉住的位：
/// ELF 标识（0..4）、class（4）、endianness（5）、e_machine（18..20）。
/// e_type（16..18）这类我们手里没有的字节交给 conf 自己（qemu / box64 / 我们
/// 生成的 conf 都只钉前几样，e_type 那字节的 mask 是 `\xfe` = ET_EXEC | ET_DYN）。
/// 要求 conf 至少钉住 class / endianness / e_machine，免得全零 mask 的 conf
/// （匹配一切）被当成候选。
fn magic_matches(magic: &[u8], mask: &[u8], info: &ElfInfo) -> bool {
    if magic.len() < 20 || mask.len() < 20 {
        return false;
    }
    if mask[4] == 0 || mask[5] == 0 || (mask[18] == 0 && mask[19] == 0) {
        return false;
    }
    let mut expected = [0u8; 20];
    expected[0..4].copy_from_slice(b"\x7fELF");
    expected[4] = match info.class {
        ElfClass::Bits32 => 1,
        ElfClass::Bits64 => 2,
    };
    expected[5] = match info.endian {
        Endian::Little => 1,
        Endian::Big => 2,
    };
    let machine = match info.endian {
        Endian::Little => info.machine.to_le_bytes(),
        Endian::Big => info.machine.to_be_bytes(),
    };
    expected[18..20].copy_from_slice(&machine);
    [0, 1, 2, 3, 4, 5, 18, 19].iter().all(|&index| {
        mask[index] == 0 || (magic[index] & mask[index]) == (expected[index] & mask[index])
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

/// 注册表里所有 guard 条目名（`aosc-exec-guard-*`）。
pub fn guard_entries(dir: &Path) -> Vec<String> {
    entry_names(dir, "aosc-exec-guard-")
}

/// 注册表里“已启用”的 qemu-* 条目名。不检查解释器文件在不在：带 `F` 的条目
/// 注册时就把解释器打开了，文件后来没了内核也照样能用（guard 自己转发才需要
/// 那个文件还在）。
pub fn enabled_qemu_entries(dir: &Path) -> Vec<String> {
    entry_names(dir, "qemu-")
        .into_iter()
        .filter(|name| {
            std::fs::read_to_string(dir.join(name))
                .is_ok_and(|text| text.lines().next() == Some("enabled"))
        })
        .collect()
}

fn entry_names(dir: &Path, prefix: &str) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut names: Vec<String> = entries
        .flatten()
        .filter_map(|entry| entry.file_name().into_string().ok())
        .filter(|name| name.starts_with(prefix))
        .collect();
    names.sort();
    names
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

    /// 造一个放 alternatives 候选 conf 的临时目录（测试并行跑，名字带 tag）。
    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("aosc-guard-alt-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// `:名字:M::magic:mask:解释器:flags`——magic/mask 取自 qemu 的 conf，只换机器码。
    fn conf_line(name: &str, interpreter: &Path, machine: &str, flags: &str) -> String {
        format!(
            ":{name}:M::\\x7fELF\\x02\\x01\\x01\\x00\\x00\\x00\\x00\\x00\\x00\\x00\\x00\\x00\
             \\x02\\x00\\x{machine}\\x00\
             :\\xff\\xff\\xff\\xff\\xff\\xff\\xff\\x00\\xff\\xff\\xff\\xff\\xff\\xff\\xff\\xff\
             \\xfe\\xff\\xff\\xff\
             :{}:{flags}\n",
            interpreter.display()
        )
    }

    /// guard 占住槽位（box64 式打包）时，模拟器只能从 alternatives 的候选里找。
    #[test]
    fn alternatives_candidates_fill_in_when_the_registry_has_nothing() {
        let dir = temp_dir("find");
        let box64 = dir.join("box64");
        let qemu = dir.join("qemu-aarch64-static");
        std::fs::write(&box64, "").unwrap();
        std::fs::write(&qemu, "").unwrap();
        std::fs::write(
            dir.join("box64.conf"),
            conf_line("box64", &box64, "3e", "P"),
        )
        .unwrap();
        std::fs::write(
            dir.join("qemu-aarch64.conf"),
            conf_line("qemu-aarch64", &qemu, "b7", "CF"),
        )
        .unwrap();
        // 槽位里就是 guard 自己的 conf：必须跳过，不然会自己转发给自己。
        std::fs::write(
            dir.join("aosc-exec-guard-aarch64.conf"),
            conf_line("aosc-exec-guard-aarch64", &qemu, "b7", "F"),
        )
        .unwrap();
        // 解释器已经不在的候选也不算数。
        std::fs::write(
            dir.join("qemu-riscv64.conf"),
            conf_line("qemu-riscv64", &dir.join("nope"), "f3", "CF"),
        )
        .unwrap();

        let aarch64 = ElfInfo {
            class: ElfClass::Bits64,
            endian: Endian::Little,
            machine: 0xb7,
        };
        let found = find_alternatives_entry(&dir, &aarch64).unwrap();
        assert_eq!(found.name, "qemu-aarch64");
        assert_eq!(found.interpreter, qemu);
        assert_eq!(found.flags, "CF");

        // 没有 qemu 名字的候选（box64 跑 x86_64）也认，取第一条命中的。
        let x86_64 = ElfInfo {
            class: ElfClass::Bits64,
            endian: Endian::Little,
            machine: 0x3e,
        };
        assert_eq!(
            find_alternatives_entry(&dir, &x86_64).unwrap().name,
            "box64"
        );

        // 谁也不命中（riscv64 的解释器不在）→ 没有候选。
        let riscv64 = ElfInfo {
            class: ElfClass::Bits64,
            endian: Endian::Little,
            machine: 0xf3,
        };
        assert!(find_alternatives_entry(&dir, &riscv64).is_none());

        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// 位宽、字节序对不上的候选不能认。
    #[test]
    fn alternatives_candidates_ignore_the_wrong_class_or_endianness() {
        let dir = temp_dir("class");
        let interpreter = dir.join("qemu-aarch64-static");
        std::fs::write(&interpreter, "").unwrap();
        std::fs::write(
            dir.join("qemu-aarch64.conf"),
            conf_line("qemu-aarch64", &interpreter, "b7", "CF"),
        )
        .unwrap();

        let bits32 = ElfInfo {
            class: ElfClass::Bits32,
            endian: Endian::Little,
            machine: 0xb7,
        };
        assert!(find_alternatives_entry(&dir, &bits32).is_none());
        let big_endian = ElfInfo {
            class: ElfClass::Bits64,
            endian: Endian::Big,
            machine: 0xb7,
        };
        assert!(find_alternatives_entry(&dir, &big_endian).is_none());

        std::fs::remove_dir_all(&dir).unwrap();
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
