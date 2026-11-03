fn main() {
    // Windows (MSVC): tauri-build embeds its app manifest only into the app
    // binary (`rustc-link-arg-bins`), so the crate's TEST executables lacked
    // the Common Controls v6 dependency and failed to start on Windows CI
    // (STATUS_ENTRYPOINT_NOT_FOUND). Instead, skip tauri-build's manifest and
    // have the linker embed the same manifest into every binary this crate
    // links — app and tests alike (one source, so no duplicate resource).
    let windows_msvc = std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows")
        && std::env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("msvc");
    let mut attrs = tauri_build::Attributes::new();
    if windows_msvc {
        let manifest = std::path::Path::new(&std::env::var("CARGO_MANIFEST_DIR").unwrap())
            .join("windows-app-manifest.xml");
        println!("cargo:rerun-if-changed={}", manifest.display());
        println!("cargo:rustc-link-arg=/MANIFEST:EMBED");
        println!("cargo:rustc-link-arg=/MANIFESTINPUT:{}", manifest.display());
        attrs = attrs.windows_attributes(tauri_build::WindowsAttributes::new_without_app_manifest());
    }
    tauri_build::try_build(attrs).expect("tauri-build failed");
}
