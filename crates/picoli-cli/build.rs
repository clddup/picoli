use std::{env, process::Command};

fn main() {
    println!("cargo:rerun-if-env-changed=RUSTC");
    let rustc = env::var_os("RUSTC").unwrap_or_else(|| "rustc".into());
    let output = Command::new(rustc)
        .arg("--version")
        .output()
        .expect("Cargo's build compiler must report its version");
    assert!(
        output.status.success(),
        "rustc --version failed during build"
    );
    println!(
        "cargo:rustc-env=PICOLI_BUILD_RUSTC={}",
        String::from_utf8(output.stdout)
            .expect("rustc version is UTF-8")
            .trim()
    );
}
