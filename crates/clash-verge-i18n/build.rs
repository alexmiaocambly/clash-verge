fn main() {
    // rust-i18n 的 `i18n!` 宏在编译期把 locales 目录嵌入二进制。
    // proc-macro 读取外部文件不会自动触发 cargo 重编，
    // 因此显式声明依赖，确保改动 locale 文件后该 crate 会重新编译。
    println!("cargo:rerun-if-changed=locales");
}
