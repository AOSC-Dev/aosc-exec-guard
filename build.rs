//! 构建脚本。
//!
//! rust-i18n 在编译期把 `locales/*.yml` 读进二进制，但它没告诉 cargo 这些文件
//! 是构建输入：只改文案、不碰 `.rs` 的话 cargo 会直接说 `Finished`，二进制里还
//! 是旧文案（实测 rust-i18n 4.2.2）。这里替它声明一下，改 yml 就会重编。

fn main() {
    println!("cargo:rerun-if-changed=locales");
}
