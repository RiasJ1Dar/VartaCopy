use clap::{Parser, Subcommand, ValueEnum};
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use vartacopy_core::{
    Conflict, Error,
    control::Control,
    journal::{self, Journal, OperationKind},
    planner::{self, ReparsePolicy, ScanLimits, ScanOptions},
    recovery, tree,
};

#[derive(Parser)]
#[command(
    name = "vartacopy",
    version,
    about = "Verified Windows file and directory copy and move"
)]
struct Args {
    /// Persistent job database (default: LOCALAPPDATA/VartaCopy/jobs.sqlite3).
    #[arg(long, global = true)]
    journal: Option<PathBuf>,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Manage the machine-wide Explorer transfer chooser (install/remove require elevation).
    Integration {
        #[arg(value_enum)]
        action: IntegrationAction,
        /// GUI executable; defaults to vartacopy-gui.exe next to this CLI.
        #[arg(long)]
        gui: Option<PathBuf>,
        /// Transfer DLL; defaults to vartacopy_transfer.dll next to the GUI.
        #[arg(long)]
        dll: Option<PathBuf>,
        #[arg(long)]
        json: bool,
    },
    /// Install, remove, or inspect per-user Explorer context menu integration.
    Explorer {
        #[arg(value_enum)]
        action: ExplorerAction,
        /// GUI executable; defaults to vartacopy-gui.exe next to this CLI.
        #[arg(long)]
        gui: Option<PathBuf>,
    },
    /// Copy a file or tree into an existing directory; directory trees are never merged.
    Copy(TransferArgs),
    /// Move a file or tree using verified publication and durable source cleanup.
    Move(TransferArgs),
    /// Save a copy or move plan without starting it.
    Enqueue {
        #[command(flatten)]
        transfer: TransferArgs,
        #[arg(long, value_enum, default_value = "copy")]
        operation: Operation,
    },
    /// Compare all data streams using BLAKE3 readback, regardless of size/mtime.
    Verify {
        left: PathBuf,
        right: PathBuf,
        #[arg(long)]
        json: bool,
    },
    /// List saved jobs (pages of up to 256).
    Jobs {
        #[arg(long, default_value_t = 50)]
        limit: usize,
        #[arg(long, default_value_t = 0)]
        offset: usize,
        #[arg(long)]
        json: bool,
    },
    /// Inspect saved counts and a page of per-entry issues.
    Show {
        job_id: i64,
        #[arg(long, default_value_t = 32)]
        limit: usize,
        #[arg(long, default_value_t = 0)]
        offset: usize,
        #[arg(long)]
        json: bool,
    },
    /// Export a consistent JSON report without replacing an existing file.
    Export {
        job_id: i64,
        path: PathBuf,
        #[arg(long)]
        json: bool,
    },
    /// Waiting jobs in execution order; optionally execute until empty or cancelled.
    Queue {
        #[arg(long)]
        run: bool,
        #[arg(long, default_value_t = 50)]
        limit: usize,
        #[arg(long, default_value_t = 0)]
        offset: usize,
        #[arg(long)]
        json: bool,
    },
    /// Schedule an unfinished job, or recheck a finished copy against recorded hashes.
    Schedule {
        job_id: i64,
        #[arg(long)]
        recheck: bool,
        #[arg(long)]
        json: bool,
    },
    /// Hold an inactive unfinished job. Owned partial data is retained.
    Pause {
        job_id: i64,
        #[arg(long)]
        json: bool,
    },
    /// Cancel an inactive unfinished job. Owned partial data is retained.
    Cancel {
        job_id: i64,
        #[arg(long)]
        json: bool,
    },
    /// Move a waiting job one place in the persisted queue.
    Order {
        job_id: i64,
        #[arg(value_enum)]
        direction: Direction,
        #[arg(long)]
        json: bool,
    },
    /// Resume or reconcile a persisted job.
    Resume {
        job_id: i64,
        #[arg(long)]
        json: bool,
    },
}
#[derive(clap::Args)]
struct TransferArgs {
    source: PathBuf,
    destination: PathBuf,
    #[arg(long)]
    dry_run: bool,
    /// Save the plan without starting it.
    #[arg(long)]
    enqueue: bool,
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
}
#[derive(Clone, Copy, ValueEnum)]
enum Operation {
    Copy,
    Move,
}
impl From<Operation> for OperationKind {
    fn from(value: Operation) -> Self {
        match value {
            Operation::Copy => Self::Copy,
            Operation::Move => Self::Move,
        }
    }
}
#[derive(Clone, Copy, ValueEnum)]
enum ExplorerAction {
    Install,
    Remove,
    Status,
}
#[derive(Clone, Copy, ValueEnum)]
enum Direction {
    Up,
    Down,
}
#[derive(Clone, Copy, ValueEnum)]
enum IntegrationAction {
    Check,
    Install,
    Remove,
    Status,
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

fn run(
    args: Args,
    cancel: &AtomicBool,
    control: &Control,
) -> Result<u8, Box<dyn std::error::Error>> {
    match args.command {
        Command::Integration {
            action,
            gui,
            dll,
            json,
        } => {
            use vartacopy_shell_windows::transfer_registration::{
                self as integration, Installation,
            };
            let gui = gui.unwrap_or(std::env::current_exe()?.with_file_name("vartacopy-gui.exe"));
            let installation = match dll {
                Some(dll) => Installation::new(&gui, &dll)?,
                None => Installation::beside(&gui)?,
            };
            let result = match action {
                IntegrationAction::Check => integration::preflight(&installation)?,
                IntegrationAction::Install => integration::install(&installation)?,
                IntegrationAction::Remove => integration::remove(&installation)?,
                IntegrationAction::Status => integration::status(&installation)?,
            };
            emit(&result, json)?;
            Ok(0)
        }
        Command::Explorer { action, gui } => {
            let gui = match gui {
                Some(path) => path,
                None => std::env::current_exe()?.with_file_name("vartacopy-gui.exe"),
            };
            match action {
                ExplorerAction::Install => {
                    vartacopy_shell_windows::registration::install(&gui)?;
                    println!(
                        "Explorer menu installed for current user (Windows 11: Show more options)"
                    );
                }
                ExplorerAction::Remove => {
                    vartacopy_shell_windows::registration::remove(&gui)?;
                    println!("VartaCopy Explorer menu removed");
                }
                ExplorerAction::Status => {
                    println!(
                        "installed={}",
                        vartacopy_shell_windows::registration::installed(&gui)?
                    );
                }
            }
            Ok(0)
        }
        Command::Copy(options) => {
            transfer(options, OperationKind::Copy, args.journal, cancel, control)
        }
        Command::Move(options) => {
            transfer(options, OperationKind::Move, args.journal, cancel, control)
        }
        Command::Enqueue {
            transfer: mut options,
            operation,
        } => {
            options.enqueue = true;
            transfer(options, operation.into(), args.journal, cancel, control)
        }
        Command::Verify { left, right, json } => {
            let streams = vartacopy_core::verify(&left, &right, cancel)?;
            emit(
                &serde_json::json!({"status":"verified", "streams_verified":streams}),
                json,
            )?;
            Ok(0)
        }
        Command::Jobs {
            limit,
            offset,
            json,
        } => {
            let path = match args.journal {
                Some(path) => path,
                None => journal::default_path()?,
            };
            let journal = Journal::open(&path)?;
            emit(&journal.jobs(limit, offset)?, json)?;
            Ok(0)
        }
        Command::Show {
            job_id,
            limit,
            offset,
            json,
        } => {
            let path = args.journal.map(Ok).unwrap_or_else(journal::default_path)?;
            emit(&Journal::open(&path)?.details(job_id, limit, offset)?, json)?;
            Ok(0)
        }
        Command::Export {
            job_id,
            path: output,
            json,
        } => {
            let path = args.journal.map(Ok).unwrap_or_else(journal::default_path)?;
            Journal::open(&path)?.export_report(job_id, &output)?;
            emit(
                &serde_json::json!({"status":"exported","job_id":job_id}),
                json,
            )?;
            Ok(0)
        }
        Command::Queue {
            run,
            limit,
            offset,
            json,
        } => {
            let path = args.journal.map(Ok).unwrap_or_else(journal::default_path)?;
            let mut journal = Journal::open(&path)?;
            if !run {
                emit(&journal.queue(limit, offset)?, json)?;
                return Ok(0);
            }
            let mut code = 0;
            // NDJSON with one result per job; no unbounded in-memory report list.
            while let Some(job) = journal.queue(1, 0)?.into_iter().next() {
                control.checkpoint()?;
                let report = recovery::run(&mut journal, job.id, control, &mut |_| Ok(()))?;
                code = code.max(report.exit_code());
                emit(&report, json)?;
                if control.is_cancelled() {
                    return Ok(130);
                }
            }
            Ok(code)
        }
        Command::Schedule {
            job_id,
            recheck,
            json,
        } => {
            let path = args.journal.map(Ok).unwrap_or_else(journal::default_path)?;
            let mut journal = Journal::open(&path)?;
            if recheck {
                journal.recheck(job_id)?;
            } else {
                journal.schedule(job_id)?;
            }
            emit(&journal.job(job_id)?, json)?;
            Ok(0)
        }
        Command::Pause { job_id, json } | Command::Cancel { job_id, json } => {
            let path = args.journal.map(Ok).unwrap_or_else(journal::default_path)?;
            let mut journal = Journal::open(&path)?;
            journal.hold(job_id, matches!(args.command, Command::Cancel { .. }))?;
            emit(&journal.job(job_id)?, json)?;
            Ok(0)
        }
        Command::Order {
            job_id,
            direction,
            json,
        } => {
            let path = args.journal.map(Ok).unwrap_or_else(journal::default_path)?;
            let mut journal = Journal::open(&path)?;
            journal.move_queued(job_id, matches!(direction, Direction::Up))?;
            emit(&journal.queue(256, 0)?, json)?;
            Ok(0)
        }
        Command::Resume { job_id, json } => {
            let path = match args.journal {
                Some(path) => path,
                None => journal::default_path()?,
            };
            let mut journal = Journal::open(&path)?;
            let report = recovery::run(&mut journal, job_id, control, &mut |_| Ok(()))?;
            let code = report.exit_code();
            emit(&report, json)?;
            Ok(code)
        }
    }
}

fn transfer(
    options: TransferArgs,
    operation: OperationKind,
    journal_path: Option<PathBuf>,
    cancel: &AtomicBool,
    control: &Control,
) -> Result<u8, Box<dyn std::error::Error>> {
    let TransferArgs {
        source,
        destination,
        dry_run,
        enqueue,
        on_conflict,
        reparse,
        max_entries,
        max_depth,
        json,
    } = options;
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
    if !plan.is_ready() && operation == OperationKind::Copy && !enqueue {
        let outcome = tree::execute(&plan, conflict, cancel)?;
        let code = outcome.exit_code();
        emit(&outcome, json)?;
        return Ok(code);
    }
    let path = match journal_path {
        Some(path) => path,
        None => journal::default_path()?,
    };
    let mut journal = Journal::open(&path)?;
    let id = journal.enqueue_with_operation(&plan, conflict, operation)?;
    if enqueue {
        emit(&journal.job(id)?, json)?;
        return Ok(0);
    }
    let report = recovery::run(&mut journal, id, control, &mut |_| Ok(()))?;
    let code = report.exit_code();
    emit(&report, json)?;
    Ok(code)
}

fn main() -> ExitCode {
    vartacopy_core::diagnostics::install_panic_hook();
    let args = Args::parse();
    let cancel = Arc::new(AtomicBool::new(false));
    let signal = cancel.clone();
    let control = Arc::new(Control::default());
    let control_signal = control.clone();
    if let Err(error) = ctrlc::set_handler(move || {
        signal.store(true, Ordering::Relaxed);
        control_signal.cancel();
    }) {
        eprintln!("vartacopy: cannot install cancellation handler: {error}");
        return ExitCode::from(1);
    }
    let json = match &args.command {
        Command::Explorer { .. } => false,
        Command::Copy(TransferArgs { json, .. })
        | Command::Move(TransferArgs { json, .. })
        | Command::Enqueue {
            transfer: TransferArgs { json, .. },
            ..
        }
        | Command::Verify { json, .. }
        | Command::Integration { json, .. }
        | Command::Jobs { json, .. }
        | Command::Resume { json, .. }
        | Command::Show { json, .. }
        | Command::Export { json, .. }
        | Command::Queue { json, .. }
        | Command::Schedule { json, .. }
        | Command::Pause { json, .. }
        | Command::Cancel { json, .. }
        | Command::Order { json, .. } => *json,
    };
    let diagnostics_enabled = matches!(
        &args.command,
        Command::Copy(TransferArgs { dry_run: false, .. })
            | Command::Move(TransferArgs { dry_run: false, .. })
            | Command::Enqueue {
                transfer: TransferArgs { dry_run: false, .. },
                ..
            }
            | Command::Resume { .. }
            | Command::Queue { run: true, .. }
    );
    let diagnostic = if diagnostics_enabled {
        let path = args
            .journal
            .clone()
            .map(Ok)
            .unwrap_or_else(journal::default_path);
        match path.and_then(|p| {
            vartacopy_core::diagnostics::Session::open(&p, "cli").map_err(Error::from)
        }) {
            Ok(session) => Some(session),
            Err(error) => {
                eprintln!("vartacopy: detailed diagnostics unavailable: {error}");
                None
            }
        }
    } else {
        None
    };
    let _diagnostic_scope = vartacopy_core::diagnostics::enter(diagnostic.clone());
    match &args.command {
        Command::Copy(options)
        | Command::Move(options)
        | Command::Enqueue {
            transfer: options, ..
        } => {
            let operation = match &args.command {
                Command::Move(_) => "move",
                Command::Enqueue {
                    operation: Operation::Move,
                    ..
                } => "enqueue_move",
                Command::Enqueue { .. } => "enqueue_copy",
                _ => "copy",
            };
            vartacopy_core::diagnostics::event(
                "cli.transfer_request",
                serde_json::json!({"operation":operation,
                "source":vartacopy_core::diagnostics::path(&options.source),"destination":vartacopy_core::diagnostics::path(&options.destination),
                "enqueue":options.enqueue,"dry_run":options.dry_run}),
            );
        }
        Command::Resume { job_id, .. } => {
            vartacopy_core::diagnostics::event("cli.resume", serde_json::json!({"job_id":job_id}))
        }
        Command::Queue { run: true, .. } => {
            vartacopy_core::diagnostics::event("cli.run_queue", serde_json::json!({}))
        }
        _ => {}
    }
    let result = run(args, &cancel, &control);
    match &result {
        Ok(code) => vartacopy_core::diagnostics::durable(
            "cli.finished",
            serde_json::json!({"exit_code":code}),
        ),
        Err(error) => vartacopy_core::diagnostics::failure(
            "cli.failed",
            error.as_ref(),
            serde_json::json!({}),
        ),
    }
    if let Some(failure) = diagnostic.as_ref().and_then(|s| s.failure()) {
        eprintln!("vartacopy: diagnostic logging stopped: {failure}");
    }
    match result {
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
