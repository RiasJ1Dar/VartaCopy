use clap::{Parser, Subcommand, ValueEnum};
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use vartacopy_core::{Conflict, Error};

#[derive(Parser)]
#[command(
    name = "vartacopy",
    version,
    about = "Verified Windows file copy (initial CLI build)"
)]
struct Args {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Copy one regular file, including ADS, into an existing directory.
    Copy {
        source: PathBuf,
        destination: PathBuf,
        #[arg(long)]
        dry_run: bool,
        #[arg(long, value_enum, default_value = "error")]
        on_conflict: Policy,
        #[arg(long)]
        json: bool,
    },
    /// Compare all data streams using BLAKE3 readback, regardless of size/mtime.
    Verify {
        left: PathBuf,
        right: PathBuf,
        #[arg(long)]
        json: bool,
    },
}
#[derive(Clone, Copy, ValueEnum)]
enum Policy {
    Error,
    Skip,
    KeepBoth,
}

fn emit<T: serde::Serialize + std::fmt::Debug>(
    value: &T,
    json: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    if json {
        println!("{}", serde_json::to_string(value)?);
    } else {
        println!("{value:#?}");
    }
    Ok(())
}

fn run(args: Args, cancel: &AtomicBool) -> Result<u8, Box<dyn std::error::Error>> {
    match args.command {
        Command::Copy {
            source,
            destination,
            dry_run,
            on_conflict,
            json,
        } => {
            if dry_run {
                emit(&vartacopy_core::plan(&source, &destination)?, json)?;
                return Ok(0);
            }
            let conflict = match on_conflict {
                Policy::Error => Conflict::Error,
                Policy::Skip => Conflict::Skip,
                Policy::KeepBoth => Conflict::KeepBoth,
            };
            let outcome = vartacopy_core::copy(&source, &destination, conflict, cancel)?;
            emit(&outcome, json)?;
            Ok(if outcome.status == "copied_verified" {
                0
            } else {
                2
            })
        }
        Command::Verify { left, right, json } => {
            let streams = vartacopy_core::verify(&left, &right, cancel)?;
            emit(
                &serde_json::json!({"status":"verified", "streams_verified":streams}),
                json,
            )?;
            Ok(0)
        }
    }
}

fn main() -> ExitCode {
    let args = Args::parse();
    let cancel = Arc::new(AtomicBool::new(false));
    let signal = cancel.clone();
    if let Err(error) = ctrlc::set_handler(move || signal.store(true, Ordering::Relaxed)) {
        eprintln!("vartacopy: cannot install cancellation handler: {error}");
        return ExitCode::from(1);
    }
    let json = match &args.command {
        Command::Copy { json, .. } | Command::Verify { json, .. } => *json,
    };
    match run(args, &cancel) {
        Ok(code) => ExitCode::from(code),
        Err(error) => {
            let cancelled = error
                .downcast_ref::<Error>()
                .is_some_and(|e| matches!(e, Error::Cancelled));
            if json {
                let message = serde_json::json!({"status":if cancelled {"cancelled"} else {"failed"}, "error":error.to_string()});
                eprintln!("{message}");
            } else {
                eprintln!("vartacopy: {error}");
            }
            ExitCode::from(if cancelled { 130 } else { 1 })
        }
    }
}
