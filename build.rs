fn main() {
    println!("cargo:rerun-if-env-changed=CARGO_FEATURE_WINDOWS_MOUNT");
    if std::env::var_os("CARGO_FEATURE_WINDOWS_MOUNT").is_none() {
        return;
    }
    let target = std::env::var("TARGET").unwrap_or_default();
    if !target.ends_with("pc-windows-msvc") {
        return;
    }
    let dll = if target.starts_with("x86_64-") {
        "winfsp-x64.dll"
    } else if target.starts_with("aarch64-") {
        "winfsp-a64.dll"
    } else {
        panic!("windows-mount requires x86_64 or aarch64 MSVC");
    };
    println!("cargo:rustc-link-lib=dylib=delayimp");
    println!("cargo:rustc-link-arg=/DELAYLOAD:{dll}");
}
