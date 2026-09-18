fn main() {
    // Keep build metadata deterministic.  SOURCE_DATE_EPOCH is intentionally the
    // only timestamp accepted by this build script; it defaults to a stable value.
    let epoch = std::env::var("SOURCE_DATE_EPOCH").unwrap_or_else(|_| "0".to_owned());
    println!("cargo:rustc-env=KIRO_BUILD_EPOCH={epoch}");
    println!("cargo:rerun-if-env-changed=SOURCE_DATE_EPOCH");
}
