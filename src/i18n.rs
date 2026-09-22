//! 多语言：文案在 `locales/*.yml`，由 rust-i18n 在编译期嵌进二进制
//! （chroot / 空 rootfs 里也能用，不依赖 /usr/share/locale，也不用带翻译文件）。
//!
//! - 一门语言一个文件（`locales/en.yml`、`locales/zh-CN.yml`）；英文是基准语言，
//!   别的语言缺翻译时回退到它（`i18n!` 的 fallback）。
//! - 代码里用 `t!(…)` / `t!(…, arg = value)` 取值，文案里的占位符写成 `%{arg}`。
//! - 加一门语言：往 `locales/` 放一份 `<locale>.yml`（照 `en.yml` 抄一遍翻掉），
//!   在 `Cargo.toml` 的 `[package.metadata.i18n] available-locales` 里加上它，
//!   再到 [`locale_for`] 里加一条前缀映射。
//! - 加一条消息：`locales/en.yml` 和 `locales/zh-CN.yml` 各加一行，代码里写
//!   `t!(…)`。`cargo test` 会核对：各语言的键是否一致、占位符是否对得上、
//!   代码里用的键是否存在、有没有谁忘了翻（见本文件底部的测试）。
//!
//! 语言在启动时（[`init`]）按一次环境变量决定：
//! `AOSC_EXEC_GUARD_LANG` > `LC_ALL` > `LC_MESSAGES` > `LANG`；
//! `zh*` 用中文，其它明确的 locale 用英文；一个都没设（服务、chroot 里很常见）
//! 默认中文。

use std::borrow::Cow;
use std::env;

use rust_i18n::t;

/// 按环境变量定下语言；读命令行之前要调一次。
pub fn init() {
    rust_i18n::set_locale(detect());
}

/// 我们的语言判定：返回 `zh-CN` / `en`。
fn detect() -> &'static str {
    let value = env::var_os("AOSC_EXEC_GUARD_LANG").or_else(|| {
        ["LC_ALL", "LC_MESSAGES", "LANG"]
            .iter()
            .find_map(env::var_os)
    });
    match value {
        Some(value) => locale_for(&value.to_string_lossy()),
        None => "zh-CN",
    }
}

/// locale 名字（`zh_CN.UTF-8`、`en_US`、`C`、`de`…）→ 我们用哪份文案；
/// 认不出来的语言先按英文来（缺翻译本来也会回退英文）。
fn locale_for(locale: &str) -> &'static str {
    let lower = locale.to_ascii_lowercase();
    let name = lower.split(['.', '@']).next().unwrap_or("");
    if name.starts_with("zh") {
        "zh-CN"
    } else {
        "en"
    }
}

/// ELF 解析失败的原因 → 文案（按原因挑键，一条条对上，不走占位符）。
pub fn not_elf_reason(reason: crate::elf::NotElfReason) -> Cow<'static, str> {
    use crate::elf::NotElfReason as R;
    match reason {
        R::CannotRead => t!("reason-cannot-read"),
        R::TooSmall => t!("reason-too-small"),
        R::NotElf => t!("reason-not-elf"),
        R::BadClass => t!("reason-bad-class"),
        R::BadEndian => t!("reason-bad-endian"),
        R::ReadFailed => t!("reason-read-failed"),
    }
}

/// 单测里钉住语言用：locale 是进程级的，测试并行跑，改它就得排队。
#[cfg(test)]
pub(crate) fn pin(locale: &str) -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let guard = LOCK.lock().unwrap_or_else(|err| err.into_inner());
    rust_i18n::set_locale(locale);
    guard
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::{BTreeMap, BTreeSet};

    #[test]
    fn locale_names_map_to_a_language() {
        assert_eq!(locale_for("zh_CN.UTF-8"), "zh-CN");
        assert_eq!(locale_for("zh_TW"), "zh-CN");
        assert_eq!(locale_for("en_US.UTF-8"), "en");
        assert_eq!(locale_for("en"), "en");
        assert_eq!(locale_for("C"), "en");
        assert_eq!(locale_for("POSIX"), "en");
        assert_eq!(locale_for("de_DE.UTF-8"), "en");
    }

    #[test]
    fn both_languages_have_text() {
        let _pin = pin("zh-CN");
        assert!(!t!("menu-run-once").is_empty());
        assert!(t!("cannot-run", path = "/tmp/x", why = "why", hint = "hint").contains("/tmp/x"));
        assert!(t!("ho-done").contains("让位"));

        rust_i18n::set_locale("en");
        assert!(t!("ho-done").contains("handover"));
    }

    /// 各语言文件的键集、每个键的占位符集都要和基准语言一致。
    #[test]
    fn every_locale_defines_the_same_keys_and_placeholders() {
        let files = locale_files();
        assert!(files.len() > 1, "至少要有一门语言");
        let (base_name, base_text) = &files[0];
        let base = parsed_with_placeholders(base_text);
        for (name, text) in &files[1..] {
            let other = parsed_with_placeholders(text);
            let base_keys: BTreeSet<_> = base.keys().collect();
            let keys: BTreeSet<_> = other.keys().collect();
            let missing: Vec<_> = base_keys.difference(&keys).collect();
            let extra: Vec<_> = keys.difference(&base_keys).collect();
            assert!(
                missing.is_empty() && extra.is_empty(),
                "{name} 与 {base_name} 的键对不上：缺 {missing:?}，多 {extra:?}"
            );
            for (key, args) in &other {
                assert_eq!(
                    args, &base[key],
                    "{name} 里 {key} 的占位符与 {base_name} 不一致"
                );
            }
        }
    }

    /// 代码里用的键必须在文案里；文案里的键也不该没人用（两边都会漏东西）。
    #[test]
    fn source_and_locale_files_agree_on_the_keys() {
        let en = parse_locale_file(&read("locales/en.yml"));
        let mut used: BTreeSet<String> = BTreeSet::new();
        for (name, text) in source_files() {
            for key in keys_in_source(&text) {
                assert!(
                    en.contains_key(&key),
                    "{name} 里用了 t!(\"{key}\")，但 locales/en.yml 里没有这条"
                );
                used.insert(key);
            }
        }
        for key in en.keys() {
            assert!(
                used.contains(key),
                "locales/en.yml 里的 {key} 没有任何地方在用"
            );
        }
    }

    // ---------- 测试用的小工具（文案语法就那么点：`键: "值"`）----------

    fn manifest_dir() -> std::path::PathBuf {
        std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
    }

    fn read(relative: &str) -> String {
        std::fs::read_to_string(manifest_dir().join(relative))
            .unwrap_or_else(|err| panic!("读不到 {relative}：{err}"))
    }

    /// `locales/` 下的每份文案（文件名、内容），按名字排好序。
    fn locale_files() -> Vec<(String, String)> {
        let mut files: Vec<_> = std::fs::read_dir(manifest_dir().join("locales"))
            .expect("读不到 locales/ 目录")
            .map(|entry| entry.expect("locales/ 里读目录项失败"))
            .map(|entry| {
                let name = entry.file_name().to_string_lossy().to_string();
                let text = std::fs::read_to_string(entry.path()).expect("读文案失败");
                (name, text)
            })
            .filter(|(name, _)| name.ends_with(".yml"))
            .collect();
        files.sort();
        files
    }

    /// `src/` 下的源文件（文件名、内容）。
    fn source_files() -> Vec<(String, String)> {
        std::fs::read_dir(manifest_dir().join("src"))
            .expect("读不到 src/ 目录")
            .map(|entry| entry.expect("src/ 里读目录项失败"))
            .map(|entry| {
                let name = entry.file_name().to_string_lossy().to_string();
                let text = std::fs::read_to_string(entry.path()).expect("读源文件失败");
                (name, text)
            })
            .filter(|(name, _)| name.ends_with(".rs"))
            .collect()
    }

    /// 文案文件 → {键: 值}；只认 `键: "值"` 这种行（我们的文件就这么写）。
    fn parse_locale_file(text: &str) -> BTreeMap<String, String> {
        let mut map = BTreeMap::new();
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') || line.starts_with("_version") {
                continue;
            }
            let Some((key, value)) = line.split_once(": ") else {
                panic!("看不懂的文案行：{line}");
            };
            map.insert(key.to_string(), value.to_string());
        }
        map
    }

    fn parsed_with_placeholders(text: &str) -> BTreeMap<String, BTreeSet<String>> {
        parse_locale_file(text)
            .into_iter()
            .map(|(key, value)| (key, placeholders_in(&value)))
            .collect()
    }

    /// 文案里的 `%{名字}`（rust-i18n 的占位符）；
    /// clap 模板的 `{usage}` 不带 `%`，不会被当成占位符。
    fn placeholders_in(value: &str) -> BTreeSet<String> {
        let mut names = BTreeSet::new();
        let mut rest = value;
        while let Some(pos) = rest.find("%{") {
            let after = &rest[pos + 2..];
            let Some(end) = after.find('}') else { break };
            names.insert(after[..end].to_string());
            rest = &after[end..];
        }
        names
    }

    /// 源码里的 `t!(…)` 键（我们只用字面量键；键也可以写在下一行）。
    fn keys_in_source(text: &str) -> Vec<String> {
        let mut keys = Vec::new();
        let bytes = text.as_bytes();
        let mut search = 0;
        while let Some(offset) = text[search..].find("t!(") {
            let start = search + offset;
            search = start + 3;
            // `format!("…")` 的尾巴也长成 `t!("…")`，看前一个字符挡掉。
            let boundary = start
                .checked_sub(1)
                .map(|i| !bytes[i].is_ascii_alphanumeric() && bytes[i] != b'_')
                .unwrap_or(true);
            if !boundary {
                continue;
            }
            let Some(after) = text[start + 3..].trim_start().strip_prefix('"') else {
                continue;
            };
            let Some(end) = after.find('"') else { break };
            let key = &after[..end];
            // 只认长得像键的（小写字母、数字、连字符）：扫描器自己源码里的
            // `"t!("`、文档里的 `t!(\"…\")` 之类会被这一步挡掉。
            if !key.is_empty()
                && key
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
            {
                keys.push(key.to_string());
            }
        }
        keys
    }
}
