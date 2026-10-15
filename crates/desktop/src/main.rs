#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
use std::path::PathBuf;
fn run() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args_os().skip(1);
    let mut path = None;
    let mut sources = Vec::new();
    while let Some(flag) = args.next() {
        if flag == "--journal" && path.is_none() {
            path = Some(PathBuf::from(
                args.next().ok_or("--journal requires a path")?,
            ));
        } else if flag == "--source" {
            sources.push(PathBuf::from(
                args.next().ok_or("--source requires a path")?,
            ));
        } else {
            return Err("Usage: vartacopy-gui [--journal PATH] [--source ABSOLUTE_PATH]...".into());
        }
    }
    let path = match path {
        Some(path) => path,
        None => vartacopy_core::journal::default_path()?,
    };
    let Some(instance) = vartacopy_shell_windows::instance::Instance::enter(&path, &sources)?
    else {
        return Ok(());
    };
    let marker = instance.marker().to_vec();
    let options = eframe::NativeOptions {
        viewport: eframe::egui::ViewportBuilder::default()
            .with_inner_size([860.0, 520.0])
            .with_min_inner_size([760.0, 480.0]),
        renderer: eframe::Renderer::Glow,
        ..Default::default()
    };
    eframe::run_native(
        "VartaCopy",
        options,
        Box::new(move |cc| {
            let mut app = vartacopy_desktop::app::App::new(cc, path)?;
            app.attach_shell(&marker, sources)?;
            Ok(Box::new(app))
        }),
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
