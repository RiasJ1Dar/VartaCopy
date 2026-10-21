fn main() {
    println!("cargo:rerun-if-changed=../../assets/vartacopy.ico");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        winresource::WindowsResource::new()
            .set_icon("../../assets/vartacopy.ico")
            .set("ProductName", "VartaCopy")
            .set("FileDescription", "VartaCopy — verified file transfers")
            .set("OriginalFilename", "vartacopy-gui.exe")
            .compile()
            .expect("compile VartaCopy Windows icon and version resources");
    }
}
