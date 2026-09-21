use std::{env, path::PathBuf, process::Command};

fn main() {
    println!("cargo:rerun-if-changed=src/windows_background.rs");
    if env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    // Embed a GUI launcher: single-exe installs need no VBScript or compiler.
    let output = PathBuf::from(env::var_os("OUT_DIR").unwrap()).join("portal-background-v1.exe");
    let mut compiler = Command::new(env::var_os("RUSTC").unwrap());
    if env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("msvc") {
        compiler.args(["-C", "target-feature=+crt-static"]);
    }
    if let Ok(linker) = env::var("RUSTC_LINKER") {
        compiler.arg("-C").arg(format!("linker={linker}"));
    }
    let status = compiler
        .args([
            "--edition=2021",
            "--crate-name",
            "portal_background",
            "--target",
        ])
        .arg(env::var_os("TARGET").unwrap())
        .args([
            "-C",
            "opt-level=s",
            "-C",
            "panic=abort",
            "-C",
            "strip=symbols",
        ])
        .arg("src/windows_background.rs")
        .arg("-o")
        .arg(output)
        .status()
        .expect("compile the Windows background launcher");
    assert!(status.success(), "Windows background launcher build failed");
}
