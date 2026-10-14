use clap::{Parser, Subcommand, ValueEnum};
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use vartacopy_core::{
    Conflict, Error,
    planner::{self, ReparsePolicy, ScanLimits, ScanOptions},
    tree,
};

#[derive(Parser)]
#[command(
    name = "vartacopy",
    version,
    about = "Verified Windows file and directory copy"
)]
struct Args {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Copy a file or tree into an existing directory; directory trees are never merged.
    Copy {
        source: PathBuf,
        destination: PathBuf,
        #[arg(long)]
        dry_run: bool,
        #[arg(long, value_enum, default_value = "error")]
        on_conflict: Policy,
        /// Refuse reparse points, or explicitly skip them with a partial result.
        #[arg(long, value_enum, default_value = "reject")]
        reparse: Links,
        /// Maximum planned entries, including directories (up to 100000).
        #[arg(long, default_value_t = 100_000)]
        max_entries: usize,
        /// Maximum relative entry depth (up to 256).
        #[arg(long, default_value_t = 128)]
        max_depth: usize,
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

#[derive(Clone, Copy, ValueEnum)]
enum Links {
    Reject,
    Skip,
}

fn emit<T: serde::Serialize + std::fmt::Debug>(
    value: &T,
    json: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    if json {
        use std::io::Write;
        let mut output = std::io::stdout().lock();
        serde_json::to_writer(&mut output, value)?;
        writeln!(output)?;
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
            reparse,
            max_entries,
            max_depth,
            json,
        } => {
            let options = ScanOptions {
                reparse: match reparse {
                    Links::Reject => ReparsePolicy::Reject,
                    Links::Skip => ReparsePolicy::Skip,
                },
                limits: ScanLimits {
                    max_entries,
                    max_depth,
                    ..ScanLimits::default()
                },
            };
            let plan = planner::scan(&source, &destination, options, cancel)?;
            if dry_run {
                let code = if plan.is_ready() { 0 } else { 2 };
                emit(&plan, json)?;
                return Ok(code);
            }
            let conflict = match on_conflict {
                Policy::Error => Conflict::Error,
                Policy::Skip => Conflict::Skip,
                Policy::KeepBoth => Conflict::KeepBoth,
            };
            let outcome = tree::execute(&plan, conflict, cancel)?;
            let code = outcome.exit_code();
            emit(&outcome, json)?;
            Ok(code)
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
