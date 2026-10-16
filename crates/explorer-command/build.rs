use std::{env, fs, path::PathBuf};

fn main() {
    if env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("msvc") {
        let exports = PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").unwrap()).join("exports.def");
        println!("cargo:rerun-if-changed={}", exports.display());
        println!("cargo:rustc-cdylib-link-arg=/DEF:{}", exports.display());
    }
    let identity = PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").unwrap())
        .join("../../specs/explorer-identity-v1.json");
    println!("cargo:rerun-if-changed={}", identity.display());
    let identity: serde_json::Value =
        serde_json::from_slice(&fs::read(identity).expect("read Explorer identity")).unwrap();
    let clsid = identity["command_clsid"].as_str().unwrap().replace('-', "");
    let clsid = u128::from_str_radix(&clsid, 16).expect("Explorer CLSID");
    let generated = format!(
        "pub const COMMAND_CLSID: windows::core::GUID = windows::core::GUID::from_u128({clsid});\n\
         pub const COMMAND_TITLE: &str = {:?};\n\
         pub const GUI_FILENAME: &str = {:?};\n",
        identity["command_title"].as_str().unwrap(),
        identity["gui_filename"].as_str().unwrap(),
    );
    fs::write(
        PathBuf::from(env::var_os("OUT_DIR").unwrap()).join("identity.rs"),
        generated,
    )
    .unwrap();
}
