use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::SystemTime;

fn main() {
    let manifest = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    let ui = manifest.join("ui");
    let inputs = source_files(&ui);
    for path in &inputs {
        println!("cargo:rerun-if-changed={}", path.display());
    }

    let modules = ui.join("node_modules");
    let lock = ui.join("package-lock.json");
    if !modules.exists() || newer_than(&lock, &modules) {
        npm(&ui, &["ci"]);
    }

    let dist = ui.join("dist").join("index.html");
    if !dist.exists() || inputs.iter().any(|path| newer_than(path, &dist)) {
        npm(&ui, &["run", "build"]);
    }
}

fn source_files(ui: &Path) -> Vec<PathBuf> {
    let mut files = vec![
        ui.join("package.json"),
        ui.join("package-lock.json"),
        ui.join("index.html"),
        ui.join("vite.config.ts"),
        ui.join("tailwind.config.cjs"),
        ui.join("postcss.config.cjs"),
        ui.join("tsconfig.json"),
    ];
    walk(&ui.join("src"), &mut files);
    files
}

fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            walk(&path, out);
        } else {
            out.push(path);
        }
    }
}

fn newer_than(left: &Path, right: &Path) -> bool {
    match (modified(left), modified(right)) {
        (Some(left), Some(right)) => left > right,
        _ => true,
    }
}

fn modified(path: &Path) -> Option<SystemTime> {
    std::fs::metadata(path).and_then(|meta| meta.modified()).ok()
}

fn npm(ui: &Path, args: &[&str]) {
    let status = Command::new("npm")
        .args(args)
        .current_dir(ui)
        .status()
        .unwrap_or_else(|error| panic!("npm is required to build the Solid web UI: {error}"));
    if !status.success() {
        panic!("npm {} failed in {}", args.join(" "), ui.display());
    }
}
