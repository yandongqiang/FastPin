use std::path::PathBuf;

fn main() {
    // 一旦输出任何 rerun-if-changed，Cargo 便不再默认监控整个包；
    // 这里显式声明构建脚本与清单，确保改动它们时也会触发重建。
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=Cargo.toml");
    // themes 目录的主题在运行时由 gpui-kit 的 ThemeRegistry 加载与监听，
    // 无需在编译期扫描或嵌入；这里只负责嵌入 exe 图标。
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    let ico = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap())
        .join("../../assets/logo/fastpin.ico");
    println!("cargo:rerun-if-changed={}", ico.display());
    let mut res = winres::WindowsResource::new();
    res.set_icon(ico.to_str().expect("fastpin.ico path"));
    res.compile().expect("embed fastpin.ico");
}
