use std::process::Command;

fn main() {
    let manifest = std::path::PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    let ui = manifest.join("ui");
    println!("cargo:rerun-if-changed=ui/src");
    println!("cargo:rerun-if-changed=ui/index.html");
    println!("cargo:rerun-if-changed=ui/package.json");
    println!("cargo:rerun-if-changed=ui/package-lock.json");
    println!("cargo:rerun-if-changed=ui/tailwind.config.cjs");
    if !ui.join("node_modules").exists() {
        let install = Command::new("npm")
            .arg("ci")
            .current_dir(&ui)
            .status()
            .expect("npm is required to build the Solid web UI");
        if !install.success() {
            panic!("npm ci failed in {}", ui.display());
        }
    }
    let build = Command::new("npm")
        .args(["run", "build"])
        .current_dir(&ui)
        .status()
        .expect("npm is required to build the Solid web UI");
    if !build.success() {
        panic!("web UI build failed");
    }
}
