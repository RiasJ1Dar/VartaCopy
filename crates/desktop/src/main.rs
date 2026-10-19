#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
use std::path::PathBuf;
fn run() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args_os().skip(1);
    let mut path = None;
    let mut sources = Vec::new();
    let mut destination = None;
    let mut operation = None;
    let mut request_id = None;
    let mut transfer_stdin = false;
    let mut selected_varta = false;
    while let Some(flag) = args.next() {
        if flag == "--journal" && path.is_none() {
            path = Some(PathBuf::from(
                args.next().ok_or("--journal requires a path")?,
            ));
        } else if flag == "--transfer-stdin" && !transfer_stdin {
            transfer_stdin = true;
        } else if flag == "--enqueue-transfer-stdin" && !transfer_stdin {
            transfer_stdin = true;
            selected_varta = true;
        } else if flag == "--source" {
            sources.push(PathBuf::from(
                args.next().ok_or("--source requires a path")?,
            ));
        } else if flag == "--destination" && destination.is_none() {
            destination = Some(PathBuf::from(
                args.next().ok_or("--destination requires a path")?,
            ));
        } else if flag == "--operation" && operation.is_none() {
            operation = Some(
                match args
                    .next()
                    .ok_or("--operation requires copy|move")?
                    .to_str()
                {
                    Some("copy") => vartacopy_shell_windows::transfer::TransferOperation::Copy,
                    Some("move") => vartacopy_shell_windows::transfer::TransferOperation::Move,
                    _ => return Err("--operation requires copy|move".into()),
                },
            );
        } else if flag == "--request-id" && request_id.is_none() {
            let value = args.next().ok_or("--request-id requires 32 hex digits")?;
            let value = value.to_str().ok_or("invalid request ID")?;
            if value.len() != 32 || !value.is_ascii() {
                return Err("--request-id requires 32 hex digits".into());
            }
            let mut id = [0; 16];
            for (index, byte) in id.iter_mut().enumerate() {
                *byte = u8::from_str_radix(&value[index * 2..index * 2 + 2], 16)?;
            }
            request_id = Some(id);
        } else {
            return Err("Usage: vartacopy-gui [--journal PATH] [--source ABSOLUTE_PATH]...".into());
        }
    }
    let path = match path {
        Some(path) => path,
        None => vartacopy_core::journal::default_path()?,
    };
    if transfer_stdin
        && (!sources.is_empty()
            || destination.is_some()
            || operation.is_some()
            || request_id.is_some())
    {
        return Err("--transfer-stdin cannot be combined with transfer arguments".into());
    }
    let transfer = if transfer_stdin {
        Some(vartacopy_shell_windows::transfer::read_transfer(
            std::io::stdin().lock(),
        )?)
    } else {
        match destination {
            Some(destination) => Some(vartacopy_shell_windows::transfer::TransferRequest {
                id: match request_id {
                    Some(id) => id,
                    None => vartacopy_shell_windows::transfer::new_transfer_id()?,
                },
                operation: operation
                    .unwrap_or(vartacopy_shell_windows::transfer::TransferOperation::Copy),
                origin: vartacopy_shell_windows::transfer::TransferOrigin::AppForm,
                sources: sources.clone(),
                destination,
            }),
            None => {
                if operation.is_some() || request_id.is_some() {
                    return Err("--operation and --request-id require --destination".into());
                }
                None
            }
        }
    };
    let elected = match &transfer {
        Some(request) if selected_varta => {
            vartacopy_shell_windows::instance::Instance::enter_selected_transfer(&path, request)?
        }
        Some(request) => {
            vartacopy_shell_windows::instance::Instance::enter_transfer(&path, request)?
        }
        None => vartacopy_shell_windows::instance::Instance::enter(&path, &sources)?,
    };
    let Some(instance) = elected else {
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
            app.attach_shell(
                &marker,
                if transfer.is_some() {
                    Vec::new()
                } else {
                    sources
                },
            )?;
            if let Some(request) = transfer {
                app.receive_transfer(if selected_varta {
                    vartacopy_shell_windows::transfer::TransferDelivery::SelectedVarta(request)
                } else {
                    vartacopy_shell_windows::transfer::TransferDelivery::Choose(request)
                })?;
            }
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
