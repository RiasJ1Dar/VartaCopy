#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
use std::path::PathBuf;
fn run() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args_os().skip(1);
    let path = match args.next() {
        Some(flag) if flag == "--journal" => args
            .next()
            .map(PathBuf::from)
            .ok_or("--journal requires a path")?,
        Some(_) => return Err("Usage: VartaCopy [--journal PATH]".into()),
        None => vartacopy_core::journal::default_path()?,
    };
    if args.next().is_some() {
        return Err("Unexpected argument".into());
    }
    let options = eframe::NativeOptions {
        viewport: eframe::egui::ViewportBuilder::default()
            .with_inner_size([1100.0, 820.0])
            .with_min_inner_size([760.0, 600.0]),
        renderer: eframe::Renderer::Glow,
        ..Default::default()
    };
    eframe::run_native(
        "VartaCopy",
        options,
        Box::new(move |cc| Ok(Box::new(vartacopy_desktop::app::App::new(cc, path)?))),
    )?;
    Ok(())
}
fn main() {
    if let Err(error) = run() {
        eprintln!("VartaCopy: {error}");
        rfd::MessageDialog::new()
            .set_title("VartaCopy")
            .set_description(format!("Не вдалося відкрити програму: {error}"))
            .set_level(rfd::MessageLevel::Error)
            .show();
        std::process::exit(1);
    }
}
