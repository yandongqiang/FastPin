use std::path::PathBuf;

fn main() {
    // 先把 themes/*.json 编译进二进制：生成的文件只含 include_str!，
    // 运行时不再扫描 exe 旁或工作目录下的 themes 目录。
    generate_included_themes();

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

/// 扫描仓库根的 themes 目录，为每个 json 生成一条 include_str! 注册项。
/// 主题文件增删只需重新构建，不用改 Rust 源码。
fn generate_included_themes() {
    let manifest = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"));
    let dir = manifest.join("../../themes");
    println!("cargo:rerun-if-changed={}", dir.display());

    let mut files: Vec<PathBuf> = match std::fs::read_dir(&dir) {
        Ok(entries) => entries
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .filter(|path| {
                path.is_file()
                    && path
                        .extension()
                        .and_then(|ext| ext.to_str())
                        .is_some_and(|ext| ext.eq_ignore_ascii_case("json"))
            })
            .collect(),
        Err(e) => {
            println!(
                "cargo:warning=找不到 themes 目录（{}），将只使用 gpui-kit 默认主题：{e}",
                dir.display()
            );
            Vec::new()
        }
    };
    files.sort();

    let mut out = String::from("static INCLUDED_THEMES: &[(&str, &str)] = &[\n");
    for path in files {
        let stem = path
            .file_stem()
            .and_then(|s| s.to_str())
            .expect("主题文件名");
        let file = path
            .file_name()
            .and_then(|s| s.to_str())
            .expect("主题文件名");
        // 字符串字面量走 Debug，文件名里即便有引号也不会破坏生成代码。
        let suffix = format!("/../../themes/{file}");
        out.push_str(&format!(
            "    ({stem:?}, include_str!(concat!(env!(\"CARGO_MANIFEST_DIR\"), {suffix:?}))),\n"
        ));
        println!("cargo:rerun-if-changed={}", path.display());
    }
    out.push_str("];\n");

    let generated =
        PathBuf::from(std::env::var("OUT_DIR").expect("OUT_DIR")).join("themes_included.rs");
    std::fs::write(&generated, out).expect("写 themes_included.rs");
}
