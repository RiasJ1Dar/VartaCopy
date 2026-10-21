use std::{path::PathBuf, process::Command};
fn git(root: &PathBuf, args: &[&str]) -> Option<String> {
    match Command::new("git").args(args).current_dir(root).output() {
        Ok(output) if output.status.success() => match String::from_utf8(output.stdout) {
            Ok(s) => Some(s.trim().to_owned()),
            Err(error) => {
                println!("cargo:warning=Git build metadata is not UTF-8: {error}");
                None
            }
        },
        Ok(output) => {
            println!(
                "cargo:warning=Git build metadata unavailable ({args:?}, exit {})",
                output.status
            );
            None
        }
        Err(error) => {
            println!("cargo:warning=Cannot read Git build metadata: {error}");
            None
        }
    }
}
fn main() {
    let root =
        PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR").expect("Cargo manifest directory"))
            .parent()
            .and_then(|p| p.parent())
            .expect("Workspace root")
            .to_path_buf();
    for name in ["HEAD", "index", "refs/heads/main"] {
        if let Some(path) = git(
            &root,
            &["rev-parse", "--path-format=absolute", "--git-path", name],
        ) {
            println!("cargo:rerun-if-changed={path}");
        }
    }
    let revision = git(&root, &["rev-parse", "HEAD"]).unwrap_or_else(|| {
        println!("cargo:warning=Git revision unavailable; diagnostics will report unknown");
        "unknown".into()
    });
    let dirty = git(
        &root,
        &["status", "--porcelain", "--untracked-files=normal"],
    )
    .map(|s| (!s.is_empty()).to_string())
    .unwrap_or_else(|| "unknown".into());
    println!("cargo:rustc-env=VARTACOPY_BUILD_REVISION={revision}");
    println!("cargo:rustc-env=VARTACOPY_BUILD_DIRTY={dirty}");
}
