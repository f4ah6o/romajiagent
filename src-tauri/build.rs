fn main() {
    #[cfg(target_os = "macos")]
    add_foundation_models_rpath();

    tauri_build::build()
}

#[cfg(target_os = "macos")]
fn add_foundation_models_rpath() {
    let Ok(out_dir) = std::env::var("OUT_DIR") else {
        return;
    };
    let Some(build_dir) = std::path::Path::new(&out_dir)
        .ancestors()
        .find(|path| path.file_name().is_some_and(|name| name == "build"))
    else {
        return;
    };

    println!("cargo:rerun-if-env-changed=APPLE_FM_SDK_SWIFT_PKG");
    if let Ok(entries) = std::fs::read_dir(build_dir) {
        for entry in entries.flatten() {
            let file_name = entry.file_name();
            let file_name = file_name.to_string_lossy();
            if !file_name.starts_with("ringo-fm-sys-") {
                continue;
            }

            let swift_build = entry.path().join("out").join("swift-build");
            let Ok(platform_dirs) = std::fs::read_dir(swift_build) else {
                continue;
            };
            for platform_dir in platform_dirs.flatten() {
                let release_dir = platform_dir.path().join("release");
                if release_dir.join("libFoundationModels.dylib").exists() {
                    println!(
                        "cargo:rustc-link-arg-bin=romaji-agent-apple-fm-sidecar=-Wl,-rpath,{}",
                        release_dir.display()
                    );
                }
            }
        }
    }
}
