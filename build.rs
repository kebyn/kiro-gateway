fn main() {
    let manifest_dir = std::path::PathBuf::from(
        std::env::var("CARGO_MANIFEST_DIR").expect("Cargo must provide CARGO_MANIFEST_DIR"),
    );
    let frontend_script = manifest_dir.join("admin-ui/src/build.mjs");
    let frontend_output = manifest_dir.join("admin-ui/dist/index.html");
    let frontend_assets =
        [manifest_dir.join("admin-ui/dist/app.js"), manifest_dir.join("admin-ui/dist/styles.css")];
    println!("cargo:rerun-if-changed={}", frontend_script.display());
    match std::process::Command::new("node")
        .arg(&frontend_script)
        .current_dir(manifest_dir.join("admin-ui"))
        .status()
    {
        Ok(status) if status.success() => {}
        Ok(status) if frontend_output.exists() => {
            println!("cargo:warning=admin UI build exited with {status}; using existing dist");
        }
        Ok(status) => panic!("admin UI build failed with {status}"),
        Err(error) if frontend_output.exists() => {
            println!("cargo:warning=Node.js unavailable ({error}); using existing dist");
        }
        Err(error) => panic!("admin UI build requires Node.js: {error}"),
    }
    println!("cargo:rerun-if-changed={}", frontend_output.display());
    for asset in frontend_assets {
        println!("cargo:rerun-if-changed={}", asset.display());
    }
    // Keep build metadata deterministic.  SOURCE_DATE_EPOCH is intentionally the
    // only timestamp accepted by this build script; it defaults to a stable value.
    let epoch = std::env::var("SOURCE_DATE_EPOCH").unwrap_or_else(|_| "0".to_owned());
    println!("cargo:rustc-env=KIRO_BUILD_EPOCH={epoch}");
    println!("cargo:rerun-if-env-changed=SOURCE_DATE_EPOCH");
}
