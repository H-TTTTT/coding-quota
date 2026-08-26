fn main() {
    println!("cargo:rerun-if-changed=assets/icon.ico");
    if std::env::var("CARGO_CFG_TARGET_OS").ok().as_deref() != Some("windows") {
        return;
    }

    let out_dir = std::path::PathBuf::from(std::env::var("OUT_DIR").expect("OUT_DIR"));
    let icon = out_dir.join("icon.ico");
    std::fs::copy("assets/icon.ico", &icon).expect("copy icon.ico");

    let rc = out_dir.join("app.rc");
    let icon_path = icon.to_string_lossy().replace('\\', "/");
    std::fs::write(&rc, format!("1 ICON \"{icon_path}\"\n")).expect("write app.rc");

    let obj = out_dir.join("app_icon.o");
    let status = std::process::Command::new("x86_64-w64-mingw32-windres")
        .args([
            "-i",
            rc.to_str().expect("rc path"),
            "-o",
            obj.to_str().expect("obj path"),
            "--output-format=coff",
        ])
        .status()
        .expect("run windres");
    assert!(status.success(), "windres failed");

    println!("cargo:rustc-link-arg-bins={}", obj.display());
}
