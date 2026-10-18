use std::{env, path::PathBuf};

fn main() {
    if env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("msvc") {
        let exports = PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").unwrap()).join("exports.def");
        println!("cargo:rerun-if-changed={}", exports.display());
        println!("cargo:rustc-cdylib-link-arg=/DEF:{}", exports.display());
    }
}
