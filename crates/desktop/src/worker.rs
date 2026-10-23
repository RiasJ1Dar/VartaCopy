//! Bounded desktop actor: planning and transfer never run on the UI thread.
use crate::telemetry::{Meter, Snapshot};
use std::{
    collections::VecDeque,
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver, SyncSender},
    },
    thread,
    time::{Duration, Instant},
};
use vartacopy_core::{
    Conflict, Error, Result,
    control::Control,
    diagnostics,
    journal::{EntryPage, Job, JobDetails, Journal, OperationKind},
    planner::{self, OperationPlan, ReparsePolicy, ScanOptions},
    recovery::{self, Progress, RunReport},
};

#[derive(Debug, Clone, Default)]
pub struct State {
    pub ready: bool,
    pub closed: bool,
    pub planning: bool,
    pub dispatching: bool,
    pub automatic: bool,
    pub active: Option<Active>,
    pub last_transfer: Option<Active>,
    pub queue: Vec<Job>,
    pub history: Vec<Job>,
    pub offset: usize,
    pub errors: VecDeque<String>,
    pub current_error: Option<String>,
    pub last_report: Option<Arc<RunReport>>,
    pub details: Option<Arc<JobDetails>>,
    pub files: Option<Arc<EntryPage>>,
    pub notice: Option<String>,
    pub windows_retry: Option<vartacopy_shell_windows::transfer::TransferRequest>,
}
#[derive(Debug, Clone)]
pub struct Active {
    pub id: i64,
    pub job: Job,
    pub entry_count: usize,
    pub progress: Option<Progress>,
    pub request: &'static str,
    pub run_started: Instant,
    pub telemetry: Snapshot,
    pub current_source: Option<PathBuf>,
}
pub enum Command {
    Dispatch {
        request: vartacopy_shell_windows::transfer::TransferRequest,
        engine: vartacopy_core::journal::DispatchEngine,
        conflict: Conflict,
    },
    Submit {
        source: PathBuf,
        destination: PathBuf,
        conflict: Conflict,
        reparse: ReparsePolicy,
        operation: OperationKind,
        start: bool,
    },
    SubmitMany {
        sources: Vec<PathBuf>,
        destination: PathBuf,
        conflict: Conflict,
        reparse: ReparsePolicy,
        operation: OperationKind,
        start: bool,
    },
    RunQueue(bool),
    Resume(i64),
    Pause(i64),
    Cancel(i64),
    Reorder {
        id: i64,
        earlier: bool,
    },
    Page(usize),
    Inspect {
        id: i64,
        offset: usize,
    },
    Files {
        id: i64,
        offset: usize,
    },
    Export {
        id: i64,
        path: PathBuf,
    },
    Recheck(i64),
    ClearErrors,
}
enum Message {
    Diagnostic(String),
    Dispatched(
        Result<(String, bool)>,
        Option<vartacopy_shell_windows::transfer::TransferRequest>,
    ),
    Wake,
    ViewTick,
    Command(Command),
    Planned(Result<Vec<OperationPlan>>, Conflict, OperationKind, bool),
    Finished(i64, Result<RunReport>),
}

#[derive(Clone, Copy)]
enum StopRequest {
    Pause,
    Cancel,
}

pub struct Client {
    sender: SyncSender<Message>,
    state: Arc<Shared>,
    shutdown: Arc<AtomicBool>,
    scan_cancel: Arc<AtomicBool>,
    active_control: Arc<Mutex<Option<Arc<Control>>>>,
}
struct Shared {
    state: Mutex<State>,
    wake: Arc<dyn Fn() + Send + Sync>,
}
fn quiet<T>(state: &Shared, f: impl FnOnce(&mut State) -> T) -> Result<T> {
    let mut guard = state
        .state
        .lock()
        .map_err(|_| Error::Journal("desktop state poisoned".into()))?;
    Ok(f(&mut guard))
}
fn edit<T>(state: &Shared, f: impl FnOnce(&mut State) -> T) -> Result<T> {
    let value = quiet(state, f)?;
    (state.wake)();
    Ok(value)
}
fn error(state: &Shared, message: impl ToString) {
    let message: String = message.to_string().chars().take(8192).collect();
    diagnostics::event("gui.error", serde_json::json!({"message":message}));
    if let Err(failure) = edit(state, |s| {
        if s.errors.len() == 32 {
            s.errors.pop_front();
        }
        s.errors.push_back(message.clone());
        s.current_error = Some(message.clone());
    }) {
        eprintln!("VartaCopy: {message}; {failure}");
    }
}
type ProgressHook = Arc<dyn Fn(&Progress) + Send + Sync>;
impl Client {
    pub fn diagnostic_message(&self, message: &str) {
        if let Err(error) = self
            .sender
            .try_send(Message::Diagnostic(message.chars().take(8192).collect()))
        {
            eprintln!("VartaCopy GUI diagnostic was not queued: {error}; {message}");
        }
    }
    pub fn start(path: PathBuf) -> std::io::Result<Self> {
        Self::start_with_notify(path, Arc::new(|| {}))
    }
    pub fn start_with_notify(
        path: PathBuf,
        wake: Arc<dyn Fn() + Send + Sync>,
    ) -> std::io::Result<Self> {
        Self::start_config(path, None, wake)
    }
    #[cfg(test)]
    fn start_with_hook(path: PathBuf, hook: Option<ProgressHook>) -> std::io::Result<Self> {
        Self::start_config(path, hook, Arc::new(|| {}))
    }
    fn start_config(
        path: PathBuf,
        hook: Option<ProgressHook>,
        wake: Arc<dyn Fn() + Send + Sync>,
    ) -> std::io::Result<Self> {
        let (sender, receiver) = mpsc::sync_channel(64);
        let state = Arc::new(Shared {
            state: Mutex::new(State::default()),
            wake,
        });
        let shutdown = Arc::new(AtomicBool::new(false));
        let scan_cancel = Arc::new(AtomicBool::new(false));
        let active_control = Arc::new(Mutex::new(None));
        let client = Self {
            sender: sender.clone(),
            state: state.clone(),
            shutdown: shutdown.clone(),
            scan_cancel: scan_cancel.clone(),
            active_control: active_control.clone(),
        };
        thread::Builder::new()
            .name("vartacopy-queue".into())
            .spawn(move || {
                let diagnostic = match diagnostics::Session::open(&path, "gui") {
                    Ok(session) => Some(session),
                    Err(e) => {
                        error(&state, format!("Детальна діагностика недоступна: {e}"));
                        None
                    }
                };
                let _diagnostic_scope = diagnostics::enter(diagnostic.clone());
                let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    actor(
                        path,
                        receiver,
                        sender,
                        &state,
                        &shutdown,
                        &scan_cancel,
                        &active_control,
                        hook,
                    )
                }));
                match outcome {
                    Ok(Ok(())) => {}
                    Ok(Err(e)) => {
                        diagnostics::failure("gui.actor_failed", &e, serde_json::json!({}));
                        error(&state, e)
                    }
                    Err(_) => error(
                        &state,
                        "Queue worker panicked; saved jobs can be recovered after restart",
                    ),
                }
                match active_control.lock() {
                    Ok(guard) => {
                        if let Some(control) = guard.as_ref() {
                            control.cancel();
                        }
                    }
                    Err(e) => error(
                        &state,
                        format!("cannot stop transfer after actor exit: {e}"),
                    ),
                }
                diagnostics::durable("session.end", serde_json::json!({"component":"gui-worker"}));
                if let Some(failure) = diagnostic.as_ref().and_then(|s| s.failure()) {
                    error(&state, format!("Запис діагностики припинився: {failure}"));
                }
                // `closed` also promises that the journal and diagnostic writer can be
                // reopened immediately, even if the final UI notification blocks.
                drop(_diagnostic_scope);
                drop(diagnostic);
                if let Err(e) = edit(&state, |s| {
                    s.closed = true;
                    s.ready = false;
                }) {
                    eprintln!("VartaCopy: {e}");
                }
            })?;
        Ok(client)
    }
    pub fn snapshot(&self) -> Result<State> {
        quiet(&self.state, |s| s.clone())
    }
    pub fn send(&self, command: Command) -> std::result::Result<(), String> {
        self.sender
            .try_send(Message::Command(command))
            .map_err(|e| format!("Queue command was not accepted: {e}"))
    }
    pub fn shutdown(&self) {
        self.shutdown.store(true, Ordering::Release);
        self.scan_cancel.store(true, Ordering::Relaxed);
        // Full means existing commands already wake recv; disconnected means the actor exited.
        match self.sender.try_send(Message::Wake) {
            Ok(())
            | Err(mpsc::TrySendError::Full(_))
            | Err(mpsc::TrySendError::Disconnected(_)) => {}
        }
        match self.active_control.lock() {
            Ok(guard) => {
                if let Some(control) = guard.as_ref() {
                    control.cancel();
                }
            }
            Err(e) => error(&self.state, format!("cannot stop transfer: {e}")),
        }
    }
}
impl Drop for Client {
    fn drop(&mut self) {
        self.shutdown();
    }
}

#[allow(clippy::too_many_arguments)]
fn scan_request(
    sources: Vec<PathBuf>,
    destination: PathBuf,
    conflict: Conflict,
    reparse: ReparsePolicy,
    operation: OperationKind,
    start: bool,
    planning: &mut Option<thread::JoinHandle<()>>,
    scan_cancel: &Arc<AtomicBool>,
    shutdown: &AtomicBool,
    state: &Shared,
    sender: &SyncSender<Message>,
) -> Result<()> {
    if planning.is_some() || shutdown.load(Ordering::Acquire) {
        return Err(Error::Journal(
            "another scan is active or the application is closing".into(),
        ));
    }
    if sources.is_empty() || sources.len() > 512 {
        return Err(Error::Journal(
            "select between 1 and 512 source roots".into(),
        ));
    }
    scan_cancel.store(false, Ordering::Relaxed);
    edit(state, |s| {
        s.planning = true;
        s.windows_retry = None;
        s.current_error = None;
        s.notice = None;
    })?;
    let tx = sender.clone();
    let cancel = scan_cancel.clone();
    let diagnostic = diagnostics::current();
    let handle = thread::Builder::new()
        .name("vartacopy-scan".into())
        .spawn(move || {
            let _diagnostic_scope=diagnostics::enter(diagnostic);
            diagnostics::event("gui.scan_request",serde_json::json!({"sources":diagnostics::paths(&sources),"destination":diagnostics::path(&destination),"operation":operation,"conflict":conflict,"reparse":reparse,"start":start}));
            let outcome = std::panic::catch_unwind(|| {
                let mut plans = Vec::with_capacity(sources.len());
                let mut remaining = planner::ScanLimits::default();
                for source in sources {
                    let plan = planner::scan(
                        &source,
                        &destination,
                        ScanOptions {
                            reparse,
                            limits: remaining,
                        },
                        &cancel,
                    )?;
                    remaining.max_entries =
                        remaining.max_entries.saturating_sub(plan.entries().len());
                    remaining.max_plan_bytes = remaining
                        .max_plan_bytes
                        .saturating_sub(plan.accounted_plan_bytes);
                    plans.push(plan);
                }
                Ok(plans)
            });
            let result = outcome.unwrap_or_else(|_| Err(Error::Journal("scanner panicked".into())));
            if let Err(e)=&result {diagnostics::failure("gui.scan_failed",e,serde_json::json!({}));}
            if let Err(e) = tx.send(Message::Planned(result, conflict, operation, start)) {
                eprintln!("VartaCopy: scan result undelivered: {e}");
            }
        });
    match handle {
        Ok(handle) => {
            *planning = Some(handle);
            Ok(())
        }
        Err(e) => {
            edit(state, |s| s.planning = false)?;
            Err(e.into())
        }
    }
}

fn refresh(journal: &Journal, state: &Shared) -> Result<()> {
    let offset = quiet(state, |s| s.offset)?;
    let active = quiet(state, |s| s.active.as_ref().map(|a| a.id))?;
    let mut queue = journal.operations(32, 0, true)?;
    queue.retain(|job| Some(job.id) != active);
    let history = journal.operations(32, offset, false)?;
    let selected = quiet(state, |s| {
        s.details.as_ref().map(|d| (d.job.id, d.issues_offset))
    })?;
    let details = selected
        .map(|(id, offset)| journal.operation_details(id, 32, offset).map(Arc::new))
        .transpose()?;
    let selected_files = quiet(state, |s| s.files.as_ref().map(|p| (p.job_id, p.offset)))?;
    let files = selected_files
        .map(|(id, offset)| journal.operation_files(id, 128, offset).map(Arc::new))
        .transpose()?;
    edit(state, |s| {
        s.queue = queue;
        s.history = history;
        s.details = details;
        s.files = files;
    })
}
#[expect(
    clippy::too_many_arguments,
    reason = "Actor owns one bounded command channel and independent cancellation handles"
)]
fn actor(
    path: PathBuf,
    receiver: Receiver<Message>,
    sender: SyncSender<Message>,
    state: &Arc<Shared>,
    shutdown: &AtomicBool,
    scan_cancel: &Arc<AtomicBool>,
    active_control: &Mutex<Option<Arc<Control>>>,
    hook: Option<ProgressHook>,
) -> Result<()> {
    let mut journal = Journal::open(&path)?;
    journal.recover_running_jobs()?;
    refresh(&journal, state)?;
    edit(state, |s| s.ready = true)?;
    let mut automatic = false;
    let mut planning: Option<thread::JoinHandle<()>> = None;
    let mut transfer: Option<thread::JoinHandle<()>> = None;
    let mut dispatch: Option<thread::JoinHandle<()>> = None;
    let unresolved = journal.unresolved_native_dispatches(32, 0)?;
    if !unresolved.receipts.is_empty() {
        edit(state, |s| {
            s.notice=Some("У журналі є незавершені запити Explorer. Перевірте їхні файли; автоматичного повтору не буде.".into())
        })?;
    }
    let mut active_id = None;
    let mut stop_request = None;
    let mut run_id = 0u64;
    let mut logging_failure_reported = false;
    loop {
        if shutdown.load(Ordering::Acquire) {
            automatic = false;
            scan_cancel.store(true, Ordering::Relaxed);
            if let Some(control) = active_control
                .lock()
                .map_err(|_| Error::Journal("transfer control poisoned".into()))?
                .as_ref()
            {
                control.cancel();
            }
            if transfer.is_none() && planning.is_none() && dispatch.is_none() {
                break;
            }
        }
        if let Ok(message) = receiver.recv() {
            let result: Result<()> = (|| {
                match message {
                    Message::Diagnostic(message) => {
                        diagnostics::event("gui.banner", serde_json::json!({"message":message}))
                    }
                    Message::Command(Command::Dispatch {
                        request,
                        engine,
                        conflict,
                    }) => {
                        if dispatch.is_some()
                            || planning.is_some()
                            || (transfer.is_some()
                                && engine == vartacopy_core::journal::DispatchEngine::Explorer)
                            || shutdown.load(Ordering::Acquire)
                        {
                            return Err(Error::Journal(
                                "Pause the current operation before choosing another engine".into(),
                            ));
                        }
                        let mut peer = journal.peer()?;
                        let tx = sender.clone();
                        let cancel = scan_cancel.clone();
                        let diagnostic_path = path.clone();
                        let diagnostic = diagnostics::current();
                        cancel.store(false, Ordering::Release);
                        edit(state, |s| {
                            s.windows_retry = None;
                            s.current_error = None;
                            s.notice = None;
                        })?;
                        let handle = thread::Builder::new()
                            .name("vartacopy-dispatch".into())
                            .spawn(move || {
                                let _diagnostic_scope = diagnostics::enter(diagnostic);
                                let outcome =
                                    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                                        crate::dispatch::run(
                                            &mut peer, &request, engine, conflict, &cancel,
                                        )
                                    }));
                                let mut retry = None;
                                let result = outcome
                                    .unwrap_or_else(|_| {
                                        Err(Error::Journal(
                                            "dispatch panicked; inspect journal before retrying"
                                                .into(),
                                        ))
                                    })
                                    .map_err(|error| {
                                        if crate::transfer_failure::windows_retry(
                                            &peer, &request, engine, &error,
                                        ) {
                                            retry = Some(request.clone());
                                        }
                                        crate::transfer_failure::describe(
                                            &peer,
                                            &diagnostic_path,
                                            &request,
                                            engine,
                                            error,
                                        )
                                    });
                                if let Err(error) = tx.send(Message::Dispatched(result, retry)) {
                                    eprintln!("VartaCopy: dispatch result undelivered: {error}");
                                }
                            })?;
                        dispatch = Some(handle);
                        edit(state, |s| s.dispatching = true)?;
                    }
                    Message::Dispatched(result, retry) => {
                        if let Some(handle) = dispatch.take() {
                            handle
                                .join()
                                .map_err(|_| Error::Journal("dispatch worker panicked".into()))?;
                        }
                        edit(state, |s| {
                            s.dispatching = false;
                            s.windows_retry = retry;
                        })?;
                        let (notice, start) = result?;
                        automatic |= start;
                        edit(state, |s| s.notice = Some(notice))?;
                        refresh(&journal, state)?;
                    }
                    Message::Wake => {}
                    Message::Command(Command::Submit {
                        source,
                        destination,
                        conflict,
                        reparse,
                        operation,
                        start,
                    }) => {
                        if dispatch.is_some() {
                            return Err(Error::Journal("engine dispatch is active".into()));
                        }
                        scan_request(
                            vec![source],
                            destination,
                            conflict,
                            reparse,
                            operation,
                            start,
                            &mut planning,
                            scan_cancel,
                            shutdown,
                            state,
                            &sender,
                        )?;
                    }
                    Message::Command(Command::SubmitMany {
                        sources,
                        destination,
                        conflict,
                        reparse,
                        operation,
                        start,
                    }) => {
                        if dispatch.is_some() {
                            return Err(Error::Journal("engine dispatch is active".into()));
                        }
                        scan_request(
                            sources,
                            destination,
                            conflict,
                            reparse,
                            operation,
                            start,
                            &mut planning,
                            scan_cancel,
                            shutdown,
                            state,
                            &sender,
                        )?;
                    }
                    Message::Planned(result, conflict, operation, start) => {
                        if let Some(handle) = planning.take() {
                            handle
                                .join()
                                .map_err(|_| Error::Journal("scanner thread panicked".into()))?;
                        }
                        edit(state, |s| s.planning = false)?;
                        let plans = result?;
                        for plan in &plans {
                            if !plan.is_ready() {
                                let problems = plan
                                    .entries()
                                    .iter()
                                    .filter_map(|entry| {
                                        entry.problem().map(|problem| {
                                            format!(
                                                "{}: {problem}",
                                                entry.relative_path().display()
                                            )
                                        })
                                    })
                                    .take(8)
                                    .collect::<Vec<_>>()
                                    .join("; ");
                                return Err(Error::Journal(format!(
                                    "Preflight blocked: {problems}"
                                )));
                            }
                        }
                        // A queued acknowledgement is visible only after this durable transaction.
                        let ids =
                            journal.enqueue_many_with_operation(&plans, conflict, operation)?;
                        diagnostics::durable(
                            "gui.enqueued",
                            serde_json::json!({"job_ids":ids,"operation":operation,"conflict":conflict}),
                        );
                        automatic |= start;
                        refresh(&journal, state)?;
                    }
                    Message::Command(Command::Inspect { id, offset }) => {
                        let details = journal.operation_details(id, 32, offset)?;
                        edit(state, |s| s.details = Some(Arc::new(details)))?;
                    }
                    Message::Command(Command::Files { id, offset }) => {
                        let files = journal.operation_files(id, 128, offset)?;
                        edit(state, |s| s.files = Some(Arc::new(files)))?;
                    }
                    Message::ViewTick => refresh(&journal, state)?,
                    Message::Command(Command::Export { id, path }) => {
                        if active_id == Some(id) {
                            return Err(Error::Journal(
                                "Pause this job before exporting a final report".into(),
                            ));
                        }
                        journal.export_operation_report(id, &path)?;
                        edit(state, |s| {
                            s.notice = Some(format!("Звіт збережено: {}", path.display()))
                        })?;
                    }
                    Message::Command(Command::Recheck(id)) => {
                        journal.recheck_operation(id)?;
                        automatic = true;
                        refresh(&journal, state)?;
                    }
                    Message::Command(Command::ClearErrors) => {
                        edit(state, |s| {
                            s.errors.clear();
                            s.current_error = None;
                            s.notice = None;
                            s.windows_retry = None;
                        })?;
                    }
                    Message::Command(Command::RunQueue(value)) => automatic = value,
                    Message::Command(Command::Resume(id)) => {
                        diagnostics::event("gui.resume", serde_json::json!({"job_id":id}));
                        if active_id == Some(id) {
                            return Err(Error::Journal("job is already active".into()));
                        }
                        journal.schedule_operation(id)?;
                        automatic = true;
                        refresh(&journal, state)?;
                    }
                    Message::Command(Command::Pause(id)) => {
                        diagnostics::event("gui.pause", serde_json::json!({"job_id":id}));
                        automatic = false;
                        if active_id == Some(id) {
                            stop_request = Some(StopRequest::Pause);
                            if let Some(control) = active_control
                                .lock()
                                .map_err(|_| Error::Journal("transfer control poisoned".into()))?
                                .as_ref()
                            {
                                control.cancel();
                            }
                            edit(state, |s| {
                                if let Some(a) = s.active.as_mut() {
                                    a.request = "pause";
                                }
                            })?;
                        } else {
                            journal.hold_operation(id, false)?;
                            refresh(&journal, state)?;
                        }
                    }
                    Message::Command(Command::Cancel(id)) => {
                        diagnostics::event("gui.cancel", serde_json::json!({"job_id":id}));
                        if active_id == Some(id) {
                            stop_request = Some(StopRequest::Cancel);
                            if let Some(control) = active_control
                                .lock()
                                .map_err(|_| Error::Journal("transfer control poisoned".into()))?
                                .as_ref()
                            {
                                control.cancel();
                            }
                            edit(state, |s| {
                                if let Some(a) = s.active.as_mut() {
                                    a.request = "cancel";
                                }
                            })?;
                        } else {
                            journal.hold_operation(id, true)?;
                            refresh(&journal, state)?;
                        }
                    }
                    Message::Command(Command::Reorder { id, earlier }) => {
                        journal.move_operation(id, earlier)?;
                        refresh(&journal, state)?;
                    }
                    Message::Command(Command::Page(offset)) => {
                        edit(state, |s| s.offset = offset)?;
                        refresh(&journal, state)?;
                    }
                    Message::Finished(id, result) => {
                        if let Some(handle) = transfer.take() {
                            handle
                                .join()
                                .map_err(|_| Error::Journal("transfer thread panicked".into()))?;
                        }
                        active_id = None;
                        *active_control
                            .lock()
                            .map_err(|_| Error::Journal("transfer control poisoned".into()))? =
                            None;
                        edit(state, |s| s.last_transfer = s.active.take())?;
                        match result {
                            Ok(mut report) => {
                                if stop_request.is_some() || shutdown.load(Ordering::Acquire) {
                                    journal.hold_operation(
                                        id,
                                        matches!(stop_request, Some(StopRequest::Cancel))
                                            || (stop_request.is_none()
                                                && shutdown.load(Ordering::Acquire)
                                                && journal.operation(id)?.operation
                                                    == OperationKind::Move),
                                    )?;
                                    report.status = journal.operation(id)?.status;
                                }
                                edit(state, |s| s.last_report = Some(Arc::new(report)))?;
                            }
                            Err(e) => {
                                diagnostics::failure(
                                    "gui.transfer_failed",
                                    &e,
                                    serde_json::json!({"job_id":id}),
                                );
                                journal.interrupt_operation(id, &e.to_string())?;
                                error(state, e);
                            }
                        }
                        stop_request = None;
                        refresh(&journal, state)?;
                    }
                }
                Ok(())
            })();
            if let Err(e) = result {
                diagnostics::failure(
                    "gui.command_failed",
                    &e,
                    serde_json::json!({"active_job":active_id}),
                );
                error(state, e);
            }
        } else {
            break;
        }
        if automatic
            && transfer.is_none()
            && dispatch.is_none()
            && !shutdown.load(Ordering::Acquire)
        {
            if let Some(job) = journal.operations(1, 0, true)?.into_iter().next() {
                let mut peer = journal.peer()?;
                let view = journal.peer()?;
                let control = Arc::new(Control::default());
                *active_control
                    .lock()
                    .map_err(|_| Error::Journal("transfer control poisoned".into()))? =
                    Some(control.clone());
                let tx = sender.clone();
                let shared = state.clone();
                let progress_hook = hook.clone();
                let id = job.id;
                run_id = run_id
                    .checked_add(1)
                    .ok_or_else(|| Error::Journal("transfer run counter exhausted".into()))?;
                let run_started = Instant::now();
                let mut meter = Meter::new(run_id);
                let source_roots = journal.operation_members(job.id)?;
                active_id = Some(id);
                edit(state, |s| {
                    s.active = Some(Active {
                        id,
                        job: Job {
                            status: "running".into(),
                            ..job.clone()
                        },
                        entry_count: job.entry_count,
                        progress: None,
                        request: "",
                        run_started,
                        telemetry: meter.snapshot(),
                        current_source: None,
                    })
                })?;
                let diagnostic = diagnostics::current();
                transfer = Some(
                    thread::Builder::new()
                        .name("vartacopy-transfer".into())
                        .spawn(move || {
                            let _diagnostic_scope = diagnostics::enter(diagnostic);
                            let mut last_progress = Instant::now();
                            let mut last_view = Instant::now();
                            let mut current_entry = None;
                            let mut current_source = None;
                            let outcome =
                                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                                    recovery::run_operation(&mut peer, id, &control, &mut |event| {
                                        meter.observe(run_started.elapsed(), &event);
                                        if current_entry != Some(event.entry) {
                                            let mut ordinal = event.entry;
                                            let root = source_roots
                                                .iter()
                                                .find(|root| {
                                                    if ordinal < root.entry_count {
                                                        true
                                                    } else {
                                                        ordinal -= root.entry_count;
                                                        false
                                                    }
                                                })
                                                .ok_or_else(|| {
                                                    Error::Journal(
                                                        "operation progress entry out of range"
                                                            .into(),
                                                    )
                                                })?;
                                            let entry = view.entry(root.id, ordinal)?;
                                            current_source =
                                                Some(if entry.relative.as_os_str().is_empty() {
                                                    root.source.clone()
                                                } else {
                                                    root.source.join(entry.relative)
                                                });
                                            current_entry = Some(event.entry);
                                        }
                                        quiet(&shared, |s| {
                                            if let Some(a) = s.active.as_mut() {
                                                a.telemetry = meter.snapshot();
                                                a.current_source = current_source.clone();
                                                a.progress = Some(event.clone());
                                            }
                                        })?;
                                        if let Some(hook) = progress_hook.as_ref() {
                                            hook(&event);
                                        }
                                        if last_progress.elapsed() >= Duration::from_millis(100) {
                                            (shared.wake)();
                                            last_progress = Instant::now();
                                        }
                                        if last_view.elapsed() >= Duration::from_millis(500) {
                                            // Only transfer events refresh the table. Full means a refresh can
                                            // coalesce with queued commands; no idle timer or UI database work.
                                            match tx.try_send(Message::ViewTick) {
                                                Ok(()) | Err(mpsc::TrySendError::Full(_)) => {}
                                                Err(mpsc::TrySendError::Disconnected(_)) => {
                                                    return Err(Error::Journal(
                                                        "file table receiver disconnected".into(),
                                                    ));
                                                }
                                            }
                                            last_view = Instant::now();
                                        }
                                        Ok(())
                                    })
                                }));
                            let result = outcome.unwrap_or_else(|_| {
                                Err(Error::Journal(
                                    "transfer panicked; resume verifies saved state".into(),
                                ))
                            });
                            if let Err(e) = tx.send(Message::Finished(id, result)) {
                                eprintln!("VartaCopy: transfer result undelivered: {e}");
                            }
                        })?,
                );
                refresh(&journal, state)?;
            } else {
                automatic = false;
            }
        }
        if shutdown.load(Ordering::Acquire) {
            automatic = false;
        }
        if quiet(state, |s| s.automatic)? != automatic {
            edit(state, |s| s.automatic = automatic)?;
        }
        if !logging_failure_reported
            && let Some(failure) = diagnostics::current().and_then(|s| s.failure())
        {
            logging_failure_reported = true;
            error(state, format!("Запис діагностики припинився: {failure}"));
        }
    }
    journal.flush()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{fs, sync::atomic::AtomicUsize, time::Instant};
    #[test]
    fn selected_roots_share_one_pause_resume_history_and_write_meter() {
        let root = tempfile::tempdir().unwrap();
        let first = root.path().join("first.txt");
        let second = root.path().join("second.txt");
        let dest = root.path().join("dest");
        let path = root.path().join("jobs.sqlite");
        fs::write(&first, b"one").unwrap();
        fs::write(&second, b"three").unwrap();
        fs::create_dir(&dest).unwrap();
        let (at_tx, at_rx) = mpsc::sync_channel(1);
        let (go_tx, go_rx) = mpsc::sync_channel(1);
        let go_rx = Mutex::new(go_rx);
        let hook: ProgressHook = Arc::new(move |event| {
            if event.entry == 0 && event.phase == recovery::Phase::Committed {
                at_tx.send(()).unwrap();
                go_rx.lock().unwrap().recv().unwrap();
            }
        });
        let client = Client::start_with_hook(path.clone(), Some(hook)).unwrap();
        wait(&client, |s| s.ready);
        client
            .send(Command::SubmitMany {
                sources: vec![first.clone(), second.clone()],
                destination: dest.clone(),
                conflict: Conflict::Error,
                reparse: ReparsePolicy::Reject,
                operation: OperationKind::Copy,
                start: true,
            })
            .unwrap();
        at_rx.recv_timeout(Duration::from_secs(10)).unwrap();
        let active = client.snapshot().unwrap().active.unwrap();
        assert_eq!(active.job.source_count, 2);
        assert_eq!(active.entry_count, 2);
        client.send(Command::Pause(active.id)).unwrap();
        wait(&client, |s| {
            s.active.as_ref().is_some_and(|a| a.request == "pause")
        });
        go_tx.send(()).unwrap();
        let paused = wait(&client, |s| {
            s.active.is_none() && s.history.first().is_some_and(|j| j.status == "paused")
        });
        assert_eq!(paused.history.len(), 1);
        assert!(paused.queue.is_empty());
        assert_eq!(paused.last_transfer.unwrap().telemetry.written, 3);
        client.shutdown();
        wait(&client, |s| s.closed);
        drop(client);
        fs::remove_file(&first).unwrap(); // A completed root must not be copied again during resume.
        let client = Client::start(path).unwrap();
        wait(&client, |s| s.ready);
        client.send(Command::Resume(active.id)).unwrap();
        let done = wait(&client, |s| {
            s.active.is_none() && s.history.first().is_some_and(|j| j.status == "completed")
        });
        assert_eq!(done.history.len(), 1);
        assert_eq!(done.last_transfer.as_ref().unwrap().telemetry.written, 5);
        assert_eq!(done.last_report.unwrap().entries_committed, 2);
        assert!(done.errors.is_empty(), "{:?}", done.errors);
        assert_eq!(fs::read(dest.join("first.txt")).unwrap(), b"one");
        assert_eq!(fs::read(dest.join("second.txt")).unwrap(), b"three");
        client.shutdown();
        wait(&client, |s| s.closed);
    }
    #[test]
    fn unavailable_diagnostics_are_reported_without_blocking_a_verified_copy() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("jobs.sqlite");
        let log = root.path().join("jobs.sqlite.diagnostics.jsonl");
        let source = root.path().join("source.txt");
        let destination = root.path().join("destination");
        fs::write(&log, b"unrelated file").unwrap();
        fs::write(&source, b"verified fixture").unwrap();
        fs::create_dir(&destination).unwrap();
        let client = Client::start(path).unwrap();
        let ready = wait(&client, |s| s.ready);
        assert!(
            ready
                .errors
                .iter()
                .any(|e| e.contains("Детальна діагностика недоступна"))
        );
        client
            .send(Command::Submit {
                source: source.clone(),
                destination: destination.clone(),
                conflict: Conflict::Error,
                reparse: ReparsePolicy::Reject,
                operation: OperationKind::Copy,
                start: true,
            })
            .unwrap();
        wait(&client, |s| {
            s.active.is_none() && s.history.first().is_some_and(|j| j.status == "completed")
        });
        client.shutdown();
        wait(&client, |s| s.closed);
        assert_eq!(fs::read(source).unwrap(), b"verified fixture");
        assert_eq!(
            fs::read(destination.join("source.txt")).unwrap(),
            b"verified fixture"
        );
        assert_eq!(fs::read(log).unwrap(), b"unrelated file");
    }
    #[test]
    fn closed_releases_diagnostic_handles_before_final_ui_notification_returns() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("jobs.sqlite");
        let slot: Arc<Mutex<Option<std::sync::Weak<Shared>>>> = Arc::new(Mutex::new(None));
        let view = slot.clone();
        let (closed_tx, closed_rx) = mpsc::sync_channel(1);
        let (release_tx, release_rx) = mpsc::sync_channel(1);
        let release_rx = Mutex::new(release_rx);
        let client = Client::start_with_notify(
            path.clone(),
            Arc::new(move || {
                let shared = view
                    .lock()
                    .unwrap()
                    .as_ref()
                    .and_then(std::sync::Weak::upgrade);
                if shared.is_some_and(|s| s.state.lock().unwrap().closed) {
                    closed_tx.send(()).unwrap();
                    release_rx.lock().unwrap().recv().unwrap();
                }
            }),
        )
        .unwrap();
        *slot.lock().unwrap() = Some(Arc::downgrade(&client.state));
        wait(&client, |s| s.ready);
        client.shutdown();
        closed_rx.recv_timeout(Duration::from_secs(15)).unwrap();
        let snapshot = client.snapshot().unwrap();
        let reopened = diagnostics::Session::open(&path, "immediate-reopen");
        release_tx.send(()).unwrap(); // Never leave the actor blocked on a failed assertion.
        assert!(snapshot.closed);
        assert!(
            reopened.is_ok(),
            "diagnostic handles survived closed: {:?}",
            reopened.err()
        );
    }
    #[test]
    fn manual_preflight_failure_is_traced_without_a_dispatch_receipt() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("jobs.sqlite");
        let source = root.path().join("source.txt");
        let destination = root.path().join("not-a-directory.txt");
        fs::write(&source, b"preserved").unwrap();
        fs::write(&destination, b"also preserved").unwrap();
        let client = Client::start(path.clone()).unwrap();
        wait(&client, |s| s.ready);
        submit(&client, source.clone(), destination.clone());
        let failed = wait(&client, |s| !s.planning && !s.errors.is_empty());
        assert!(failed.queue.is_empty() && failed.history.is_empty());
        client.shutdown();
        wait(&client, |s| s.closed);
        drop(client);
        let entries = fs::read_to_string(path.with_file_name("jobs.sqlite.diagnostics.jsonl"))
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
            .collect::<Vec<_>>();
        assert!(entries.iter().any(|e| e["event"] == "gui.scan_request"));
        assert!(entries.iter().any(|e| e["event"] == "gui.scan_failed"));
        assert!(entries.iter().any(|e| e["event"] == "filesystem.error"
            && e["data"]["context"]["api"] == "open_destination_directory"));
        assert_eq!(fs::read(source).unwrap(), b"preserved");
        assert_eq!(fs::read(destination).unwrap(), b"also preserved");
    }
    #[test]
    fn failed_dispatch_preflight_preserves_sources_and_records_request_without_success_job() {
        use vartacopy_shell_windows::transfer::{
            TransferOperation, TransferOrigin, TransferRequest,
        };
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("jobs.sqlite");
        let source = root.path().join("дані-😀.txt");
        fs::write(&source, b"untouched source").unwrap();
        let destination = root.path().join("destination-file");
        fs::write(&destination, b"not a directory").unwrap();
        let client = Client::start(path.clone()).unwrap();
        wait(&client, |s| s.ready);
        client
            .send(Command::Dispatch {
                request: TransferRequest {
                    id: [51; 16],
                    operation: TransferOperation::Move,
                    origin: TransferOrigin::Explorer,
                    sources: vec![source.clone()],
                    destination: destination.clone(),
                },
                engine: vartacopy_core::journal::DispatchEngine::VartaCopy,
                conflict: Conflict::Error,
            })
            .unwrap();
        let failed = wait(&client, |s| !s.dispatching && !s.errors.is_empty());
        assert!(
            failed
                .errors
                .back()
                .unwrap()
                .contains("Переміщення не розпочато")
        );
        assert!(failed.errors.back().unwrap().contains("Звіт:"));
        assert!(failed.queue.is_empty() && failed.history.is_empty());
        assert!(failed.active.is_none());
        assert_eq!(fs::read(&source).unwrap(), b"untouched source");
        assert_eq!(fs::read(&destination).unwrap(), b"not a directory");
        assert_eq!(failed.current_error.as_ref(), failed.errors.back());
        let valid_destination = root.path().join("valid-destination");
        fs::create_dir(&valid_destination).unwrap();
        client
            .send(Command::Dispatch {
                request: TransferRequest {
                    id: [52; 16],
                    operation: TransferOperation::Copy,
                    origin: TransferOrigin::Explorer,
                    sources: vec![source.clone()],
                    destination: valid_destination.clone(),
                },
                engine: vartacopy_core::journal::DispatchEngine::VartaCopy,
                conflict: Conflict::Error,
            })
            .unwrap();
        let completed = wait(&client, |s| {
            !s.dispatching
                && s.active.is_none()
                && s.history.iter().any(|j| j.status == "completed")
        });
        assert!(completed.current_error.is_none());
        assert_eq!(completed.errors, failed.errors); // Historical evidence remains available.
        assert_eq!(
            fs::read(valid_destination.join(source.file_name().unwrap())).unwrap(),
            b"untouched source"
        );
        client.shutdown();
        wait(&client, |s| s.closed);
        drop(client);
        let journal = Journal::open(&path).unwrap();
        assert!(journal.dispatch_receipt([51; 16]).unwrap().is_none());
        assert_eq!(journal.jobs(10, 0).unwrap().len(), 1);
        assert!(journal.dispatch_receipt([52; 16]).unwrap().is_some());
        let report: serde_json::Value = serde_json::from_slice(
            &fs::read(path.with_file_name("jobs.sqlite.last-transfer-error.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(report["acceptance"], "not_accepted");
        assert_eq!(report["request"]["id"], serde_json::json!(vec![51; 16]));
    }
    #[test]
    fn dispatch_choice_runs_varta_once_and_replay_does_not_rescan_or_restart() {
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("dispatch.txt");
        let destination = root.path().join("target");
        fs::write(&source, b"actor dispatch fixture").unwrap();
        fs::create_dir(&destination).unwrap();
        let client = Client::start(root.path().join("jobs.sqlite")).unwrap();
        wait(&client, |s| s.ready);
        let request = vartacopy_shell_windows::transfer::TransferRequest {
            id: [7; 16],
            operation: vartacopy_shell_windows::transfer::TransferOperation::Copy,
            origin: vartacopy_shell_windows::transfer::TransferOrigin::AppForm,
            sources: vec![source.clone()],
            destination: destination.clone(),
        };
        client
            .send(Command::Dispatch {
                request: request.clone(),
                engine: vartacopy_core::journal::DispatchEngine::VartaCopy,
                conflict: Conflict::Error,
            })
            .unwrap();
        wait(&client, |s| {
            s.history.first().is_some_and(|j| j.status == "completed")
                && !s.dispatching
                && s.active.is_none()
        });
        assert_eq!(
            fs::read(destination.join("dispatch.txt")).unwrap(),
            b"actor dispatch fixture"
        );
        fs::remove_file(&source).unwrap();
        client
            .send(Command::Dispatch {
                request,
                engine: vartacopy_core::journal::DispatchEngine::VartaCopy,
                conflict: Conflict::Error,
            })
            .unwrap();
        let state = wait(&client, |s| {
            s.notice
                .as_ref()
                .is_some_and(|n| n.contains("Повторного запуску"))
                && !s.dispatching
        });
        assert_eq!(state.history.len(), 1);
        assert!(state.errors.is_empty());
        assert!(state.active.is_none());
        client.shutdown();
        wait(&client, |s| s.closed);
    }
    #[track_caller]
    fn wait(client: &Client, condition: impl Fn(&State) -> bool) -> State {
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            let state = client.snapshot().unwrap();
            if condition(&state) {
                return state;
            }
            assert!(Instant::now() < deadline, "worker timeout: state={state:?}");
            thread::sleep(Duration::from_millis(5));
        }
    }
    #[track_caller]
    fn wait_for_transfer(client: &Client, condition: impl Fn(&State) -> bool) -> State {
        let hard_deadline = Instant::now() + Duration::from_secs(120);
        let mut idle_deadline = Instant::now() + Duration::from_secs(15);
        let mut previous = None;
        loop {
            let state = client.snapshot().unwrap();
            if condition(&state) {
                return state;
            }
            let cursor = state.active.as_ref().and_then(|a| {
                a.progress.as_ref().map(|p| {
                    (
                        a.telemetry.run_id,
                        a.id,
                        p.entry,
                        p.phase,
                        p.stream_index,
                        p.bytes,
                    )
                })
            });
            if cursor.is_some() && cursor != previous {
                idle_deadline = Instant::now() + Duration::from_secs(15);
            }
            previous = cursor;
            assert!(
                Instant::now() < idle_deadline && Instant::now() < hard_deadline,
                "transfer stalled or exceeded its total bound: state={state:?}"
            );
            thread::sleep(Duration::from_millis(5));
        }
    }
    fn submit(client: &Client, source: PathBuf, destination: PathBuf) {
        client
            .send(Command::Submit {
                source,
                destination,
                conflict: Conflict::Error,
                reparse: ReparsePolicy::Reject,
                operation: OperationKind::Copy,
                start: false,
            })
            .unwrap();
    }
    #[test]
    fn queues_durably_during_transfer_pauses_resumes_and_restarts() {
        let root = tempfile::tempdir().unwrap();
        let dest = root.path().join("dest");
        fs::create_dir(&dest).unwrap();
        let source = root.path().join("first");
        fs::write(&source, vec![91; 5 * 1024 * 1024]).unwrap();
        let second = root.path().join("second");
        fs::write(&second, b"second").unwrap();
        let second = second.canonicalize().unwrap();
        let path = root.path().join("jobs.sqlite");
        let (ready_tx, ready_rx) = mpsc::sync_channel(1);
        let (go_tx, go_rx) = mpsc::sync_channel(1);
        let go_rx = Mutex::new(go_rx);
        let calls = AtomicUsize::new(0);
        let hook: ProgressHook = Arc::new(move |event| {
            if event.phase == recovery::Phase::CheckpointCommitted
                && calls.fetch_add(1, Ordering::SeqCst) == 0
            {
                ready_tx.send(()).unwrap();
                go_rx.lock().unwrap().recv().unwrap();
            }
        });
        let client = Client::start_with_hook(path.clone(), Some(hook)).unwrap();
        wait(&client, |s| s.ready);
        submit(&client, source.clone(), dest.clone());
        let queued = wait(&client, |s| s.queue.len() == 1);
        let first_id = queued.queue[0].id;
        client.send(Command::RunQueue(true)).unwrap();
        ready_rx.recv_timeout(Duration::from_secs(10)).unwrap();
        submit(&client, second.clone(), dest.clone());
        wait(&client, |s| {
            !s.planning && s.queue.len() == 1 && s.queue[0].source == second
        });
        client.send(Command::Pause(first_id)).unwrap();
        wait(&client, |s| {
            s.active.as_ref().is_some_and(|a| a.request == "pause")
        });
        go_tx.send(()).unwrap();
        wait(&client, |s| {
            s.active.is_none()
                && s.history
                    .iter()
                    .any(|j| j.id == first_id && j.status == "paused")
        });
        assert!(!dest.join("first").exists());
        assert!(!dest.join("second").exists());
        client.shutdown();
        wait(&client, |s| s.closed);
        drop(client);
        let client = Client::start(path.clone()).unwrap();
        let recovered = wait(&client, |s| s.ready);
        assert!(!recovered.automatic);
        assert_eq!(recovered.queue.len(), 1);
        client.send(Command::Resume(first_id)).unwrap();
        let done = wait(&client, |s| {
            !s.automatic
                && s.history.len() == 2
                && s.history.iter().all(|j| j.status == "completed")
        });
        assert!(done.errors.is_empty(), "{:?}", done.errors);
        assert_eq!(
            fs::read(dest.join("first")).unwrap(),
            fs::read(source).unwrap()
        );
        assert_eq!(fs::read(dest.join("second")).unwrap(), b"second");
        client.shutdown();
        wait(&client, |s| s.closed);
    }
    #[test]
    fn selected_varta_dispatch_queues_while_a_previous_copy_is_running() {
        use vartacopy_shell_windows::transfer::{
            TransferOperation, TransferOrigin, TransferRequest,
        };
        let root = tempfile::tempdir().unwrap();
        let dest = root.path().join("dest");
        fs::create_dir(&dest).unwrap();
        let first = root.path().join("first");
        fs::write(&first, vec![11; 5 * 1024 * 1024]).unwrap();
        let second = root.path().join("second");
        fs::write(&second, b"queued shell request").unwrap();
        let second = second.canonicalize().unwrap();
        let (ready_tx, ready_rx) = mpsc::sync_channel(1);
        let (go_tx, go_rx) = mpsc::sync_channel(1);
        let go_rx = Mutex::new(go_rx);
        let once = AtomicBool::new(false);
        let hook: ProgressHook = Arc::new(move |event| {
            if event.phase == recovery::Phase::CheckpointCommitted
                && !once.swap(true, Ordering::SeqCst)
            {
                ready_tx.send(()).unwrap();
                go_rx
                    .lock()
                    .unwrap()
                    .recv_timeout(Duration::from_secs(15))
                    .unwrap();
            }
        });
        let client = Client::start_with_hook(root.path().join("jobs.sqlite"), Some(hook)).unwrap();
        // Drop this guard before Client even if an assertion fails while the copy is held.
        struct Release(Option<mpsc::SyncSender<()>>);
        impl Drop for Release {
            fn drop(&mut self) {
                if let Some(tx) = self.0.take()
                    && let Err(error) = tx.send(())
                {
                    eprintln!("test release failed: {error}");
                }
            }
        }
        let release = Release(Some(go_tx));
        wait(&client, |s| s.ready);
        submit(&client, first.clone(), dest.clone());
        wait(&client, |s| !s.planning && s.queue.len() == 1);
        client.send(Command::RunQueue(true)).unwrap();
        ready_rx.recv_timeout(Duration::from_secs(10)).unwrap();
        client
            .send(Command::Dispatch {
                request: TransferRequest {
                    id: [77; 16],
                    operation: TransferOperation::Copy,
                    origin: TransferOrigin::Explorer,
                    sources: vec![second.clone()],
                    destination: dest.clone(),
                },
                engine: vartacopy_core::journal::DispatchEngine::VartaCopy,
                conflict: Conflict::Error,
            })
            .unwrap();
        let queued = wait(&client, |s| {
            !s.dispatching && s.queue.iter().any(|j| j.source == second)
        });
        assert!(queued.active.is_some());
        assert!(!dest.join("second").exists());
        drop(release);
        let done = wait(&client, |s| {
            s.active.is_none()
                && s.history.len() == 2
                && s.history.iter().all(|j| j.status == "completed")
        });
        assert!(done.errors.is_empty(), "{:?}", done.errors);
        assert_eq!(
            fs::read(dest.join("first")).unwrap(),
            fs::read(first).unwrap()
        );
        assert_eq!(
            fs::read(dest.join("second")).unwrap(),
            b"queued shell request"
        );
        client.shutdown();
        wait(&client, |s| s.closed);
    }
    #[test]
    fn blocked_scan_and_cancelled_queue_are_explicit_and_persisted() {
        let root = tempfile::tempdir().unwrap();
        let dest = root.path().join("dest");
        fs::create_dir(&dest).unwrap();
        let path = root.path().join("jobs.sqlite");
        let client = Client::start(path.clone()).unwrap();
        wait(&client, |s| s.ready);
        submit(&client, root.path().join("missing"), dest.clone());
        let failed = wait(&client, |s| !s.errors.is_empty());
        assert!(failed.queue.is_empty());
        let source = root.path().join("file");
        fs::write(&source, b"retained").unwrap();
        submit(&client, source, dest.clone());
        let id = wait(&client, |s| s.queue.len() == 1).queue[0].id;
        client.send(Command::Cancel(id)).unwrap();
        wait(&client, |s| {
            s.queue.is_empty() && s.history[0].status == "cancelled"
        });
        client.shutdown();
        wait(&client, |s| s.closed);
        let journal = Journal::open(&path).unwrap();
        assert_eq!(journal.job(id).unwrap().status, "cancelled");
        assert_eq!(fs::read_dir(dest).unwrap().count(), 0);
    }
    #[test]
    fn shutdown_during_transfer_keeps_a_resumable_job_and_releases_lease() {
        let root = tempfile::tempdir().unwrap();
        let dest = root.path().join("dest");
        fs::create_dir(&dest).unwrap();
        let source = root.path().join("data");
        fs::write(&source, vec![32; 5 * 1024 * 1024]).unwrap();
        let path = root.path().join("jobs.sqlite");
        let (ready_tx, ready_rx) = mpsc::sync_channel(1);
        let (go_tx, go_rx) = mpsc::sync_channel(1);
        let go_rx = Mutex::new(go_rx);
        let once = AtomicBool::new(false);
        let hook: ProgressHook = Arc::new(move |event| {
            if event.phase == recovery::Phase::CheckpointCommitted
                && !once.swap(true, Ordering::SeqCst)
            {
                ready_tx.send(()).unwrap();
                go_rx.lock().unwrap().recv().unwrap();
            }
        });
        let client = Client::start_with_hook(path.clone(), Some(hook)).unwrap();
        wait(&client, |s| s.ready);
        submit(&client, source.clone(), dest.clone());
        let id = wait(&client, |s| s.queue.len() == 1).queue[0].id;
        client.send(Command::RunQueue(true)).unwrap();
        ready_rx.recv_timeout(Duration::from_secs(10)).unwrap();
        client.shutdown();
        go_tx.send(()).unwrap();
        wait(&client, |s| s.closed);
        let mut journal = Journal::open(&path).unwrap();
        assert_eq!(journal.job(id).unwrap().status, "paused");
        let report = recovery::run(&mut journal, id, &Control::default(), &mut |_| Ok(())).unwrap();
        assert_eq!(report.status, "completed");
        assert_eq!(report.resumed_bytes, 4 * 1024 * 1024);
        assert_eq!(
            fs::read(source).unwrap(),
            fs::read(dest.join("data")).unwrap()
        );
    }
    #[test]
    fn inspect_export_and_recheck_commands_keep_persisted_failure_details() {
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("file");
        let dest = root.path().join("dest");
        fs::write(&source, b"good").unwrap();
        fs::create_dir(&dest).unwrap();
        let path = root.path().join("jobs.sqlite");
        let id = {
            let mut journal = Journal::open(&path).unwrap();
            let plan = planner::scan(
                &source,
                &dest,
                ScanOptions::default(),
                &AtomicBool::new(false),
            )
            .unwrap();
            let id = journal.enqueue(&plan, Conflict::Error).unwrap();
            assert_eq!(
                recovery::run(&mut journal, id, &Control::default(), &mut |_| Ok(()))
                    .unwrap()
                    .status,
                "completed"
            );
            id
        };
        fs::write(dest.join("file"), b"bad!").unwrap();
        let client = Client::start(path).unwrap();
        wait(&client, |s| s.ready);
        client.send(Command::Recheck(id)).unwrap();
        wait(&client, |s| {
            s.history[0].status == "interrupted" && s.active.is_none()
        });
        client.send(Command::Inspect { id, offset: 0 }).unwrap();
        let details = wait(&client, |s| {
            s.details.as_ref().is_some_and(|d| d.issues.len() == 1)
        });
        assert_eq!(details.details.unwrap().job.id, id);
        let output = root.path().join("report.json");
        client
            .send(Command::Export {
                id,
                path: output.clone(),
            })
            .unwrap();
        wait(&client, |s| s.notice.is_some());
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&fs::read(output).unwrap()).unwrap()["summary"]
                ["job"]["status"],
            "interrupted"
        );
        client.send(Command::ClearErrors).unwrap();
        wait(&client, |s| s.notice.is_none());
        fs::write(dest.join("file"), b"good").unwrap();
        client.send(Command::Resume(id)).unwrap();
        wait(&client, |s| {
            s.details
                .as_ref()
                .is_some_and(|d| d.job.status == "completed" && d.issues.is_empty())
        });
        client.shutdown();
        wait(&client, |s| s.closed);
    }
    #[test]
    fn snapshot_reads_and_idle_actor_do_not_wake_ui_but_shutdown_does() {
        let root = tempfile::tempdir().unwrap();
        let (tx, rx) = mpsc::channel();
        let client = Client::start_with_notify(
            root.path().join("jobs.sqlite"),
            Arc::new(move || tx.send(()).unwrap()),
        )
        .unwrap();
        // Initial history and readiness each publish one state change.
        rx.recv_timeout(Duration::from_secs(5)).unwrap();
        rx.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(client.snapshot().unwrap().ready);
        for _ in 0..100 {
            assert!(client.snapshot().unwrap().ready);
        }
        assert!(
            matches!(
                rx.recv_timeout(Duration::from_millis(80)),
                Err(mpsc::RecvTimeoutError::Timeout)
            ),
            "idle UI must not be polled"
        );
        client.shutdown();
        rx.recv_timeout(Duration::from_secs(1)).unwrap();
        assert!(wait(&client, |s| s.closed).closed);
    }

    #[test]
    fn submit_and_run_waits_for_durable_plan_and_refreshes_the_selected_file_window() {
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("tree");
        let dest = root.path().join("dest");
        fs::create_dir(&source).unwrap();
        fs::create_dir(&dest).unwrap();
        for i in 0..140 {
            fs::write(source.join(format!("file-{i:03}")), b"data").unwrap();
        }
        let client = Client::start(root.path().join("jobs.sqlite")).unwrap();
        wait(&client, |s| s.ready);
        client
            .send(Command::Submit {
                source,
                destination: dest,
                conflict: Conflict::Error,
                reparse: ReparsePolicy::Reject,
                operation: OperationKind::Copy,
                start: true,
            })
            .unwrap();
        let done = wait_for_transfer(&client, |s| {
            s.history.first().is_some_and(|j| j.status == "completed") && s.active.is_none()
        });
        assert!(done.errors.is_empty(), "{:?}", done.errors);
        let id = done.history[0].id;
        client.send(Command::Files { id, offset: 128 }).unwrap();
        let page = wait(&client, |s| {
            s.files.as_ref().is_some_and(|p| p.offset == 128)
        });
        let page = page.files.unwrap();
        assert_eq!(page.total, 141);
        assert_eq!(page.entries.len(), 13);
        assert!(
            page.entries
                .iter()
                .all(|e| e.state == vartacopy_core::journal::EntryState::Committed)
        );
        let old_report = done.last_report.unwrap();
        client.send(Command::Recheck(id)).unwrap();
        wait_for_transfer(&client, |s| {
            s.last_report
                .as_ref()
                .is_some_and(|r| r.job_id == id && !Arc::ptr_eq(r, &old_report))
                && !s.automatic
        });
        client.shutdown();
        wait(&client, |s| s.closed);
    }
    #[test]
    fn multiple_roots_copy_together_and_a_missing_root_saves_nothing() {
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("a");
        let second = root.path().join("b");
        let dest = root.path().join("dest");
        fs::create_dir(&dest).unwrap();
        fs::write(&source, b"one").unwrap();
        fs::write(&second, b"two").unwrap();
        let client = Client::start(root.path().join("jobs.sqlite")).unwrap();
        wait(&client, |s| s.ready);
        client
            .send(Command::SubmitMany {
                sources: vec![source.clone(), root.path().join("missing")],
                destination: dest.clone(),
                conflict: Conflict::Error,
                reparse: ReparsePolicy::Reject,
                operation: OperationKind::Copy,
                start: true,
            })
            .unwrap();
        let failed = wait(&client, |s| !s.errors.is_empty() && !s.planning);
        assert!(failed.history.is_empty());
        assert!(failed.queue.is_empty());
        assert!(fs::read_dir(&dest).unwrap().next().is_none());
        client.send(Command::ClearErrors).unwrap();
        client
            .send(Command::SubmitMany {
                sources: vec![source, second],
                destination: dest.clone(),
                conflict: Conflict::Error,
                reparse: ReparsePolicy::Reject,
                operation: OperationKind::Copy,
                start: true,
            })
            .unwrap();
        let done = wait(&client, |s| {
            s.history.len() == 1
                && s.history.iter().all(|j| j.status == "completed")
                && s.active.is_none()
        });
        assert!(done.errors.is_empty(), "{:?}", done.errors);
        assert_eq!(fs::read(dest.join("a")).unwrap(), b"one");
        assert_eq!(fs::read(dest.join("b")).unwrap(), b"two");
        client.shutdown();
        wait(&client, |s| s.closed);
    }

    #[test]
    fn move_planning_reorder_and_restart_preserve_operation_alongside_copy() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("jobs.sqlite");
        let dest = root.path().join("dest");
        fs::create_dir(&dest).unwrap();
        let a = root.path().join("move-a");
        let b = root.path().join("move-b");
        let copy = root.path().join("copy");
        for source in [&a, &b, &copy] {
            fs::write(source, b"durable operation").unwrap();
        }
        let client = Client::start(path.clone()).unwrap();
        wait(&client, |s| s.ready);
        client
            .send(Command::Submit {
                source: a.clone(),
                destination: dest.clone(),
                conflict: Conflict::Error,
                reparse: ReparsePolicy::Reject,
                operation: OperationKind::Move,
                start: false,
            })
            .unwrap();
        wait(&client, |s| !s.planning && s.queue.len() == 1);
        client
            .send(Command::SubmitMany {
                sources: vec![b.clone()],
                destination: dest.clone(),
                conflict: Conflict::Error,
                reparse: ReparsePolicy::Reject,
                operation: OperationKind::Move,
                start: false,
            })
            .unwrap();
        wait(&client, |s| !s.planning && s.queue.len() == 2);
        submit(&client, copy.clone(), dest.clone());
        let queued = wait(&client, |s| !s.planning && s.queue.len() == 3);
        assert_eq!(
            queued.queue.iter().map(|j| j.operation).collect::<Vec<_>>(),
            [
                OperationKind::Move,
                OperationKind::Move,
                OperationKind::Copy
            ]
        );
        let copy_id = queued.queue[2].id;
        client
            .send(Command::Reorder {
                id: copy_id,
                earlier: true,
            })
            .unwrap();
        wait(&client, |s| s.queue[1].id == copy_id);
        client.shutdown();
        wait(&client, |s| s.closed);
        drop(client);
        let client = Client::start(path).unwrap();
        let restored = wait(&client, |s| s.ready);
        assert_eq!(
            restored
                .queue
                .iter()
                .map(|j| j.operation)
                .collect::<Vec<_>>(),
            [
                OperationKind::Move,
                OperationKind::Copy,
                OperationKind::Move
            ]
        );
        client.send(Command::RunQueue(true)).unwrap();
        let done = wait(&client, |s| {
            !s.automatic && s.active.is_none() && s.history.len() == 3 && s.queue.is_empty()
        });
        assert!(done.errors.is_empty(), "{:?}", done.errors);
        for job in &done.history {
            assert_eq!(
                fs::read(dest.join(job.source.file_name().unwrap())).unwrap(),
                b"durable operation"
            );
            match (job.operation, job.status.as_str()) {
                (OperationKind::Move, "completed") => assert!(!job.source.exists()),
                (OperationKind::Move, "partial") => assert!(job.source.exists()),
                (OperationKind::Copy, "completed") => assert!(job.source.exists()),
                _ => panic!("unexpected durable outcome: {job:?}"),
            }
        }
        client.shutdown();
        wait(&client, |s| s.closed);
    }

    #[test]
    fn cleanup_stop_intent_distinguishes_user_pause_cancel_and_shutdown_and_resumes() {
        for (request, expected) in [
            ("pause", "paused"),
            ("cancel", "cancelled"),
            ("shutdown", "cancelled"),
        ] {
            let root = tempfile::tempdir().unwrap();
            let source = root.path().join("move");
            let dest = root.path().join("dest");
            let path = root.path().join("jobs.sqlite");
            fs::create_dir(&dest).unwrap();
            fs::write(&source, b"stopped move").unwrap();
            let (ready_tx, ready_rx) = mpsc::sync_channel(1);
            let (go_tx, go_rx) = mpsc::sync_channel(1);
            let go_rx = Mutex::new(go_rx);
            let once = AtomicBool::new(false);
            let hook: ProgressHook = Arc::new(move |event| {
                if event.phase == recovery::Phase::CleanupIntent
                    && !once.swap(true, Ordering::SeqCst)
                {
                    ready_tx.send(()).unwrap();
                    go_rx.lock().unwrap().recv().unwrap();
                }
            });
            let client = Client::start_with_hook(path.clone(), Some(hook)).unwrap();
            wait(&client, |s| s.ready);
            client
                .send(Command::Submit {
                    source: source.clone(),
                    destination: dest.clone(),
                    conflict: Conflict::Error,
                    reparse: ReparsePolicy::Reject,
                    operation: OperationKind::Move,
                    start: true,
                })
                .unwrap();
            ready_rx.recv_timeout(Duration::from_secs(10)).unwrap();
            let id = client.snapshot().unwrap().active.unwrap().id;
            match request {
                "pause" => client.send(Command::Pause(id)).unwrap(),
                "cancel" => client.send(Command::Cancel(id)).unwrap(),
                "shutdown" => client.shutdown(),
                _ => unreachable!(),
            }
            if request != "shutdown" {
                wait(&client, |s| {
                    s.active.as_ref().is_some_and(|a| a.request == request)
                });
            }
            go_tx.send(()).unwrap();
            let done = wait(&client, |s| {
                s.active.is_none() && s.history.iter().any(|j| j.id == id && j.status == expected)
            });
            let report = done.last_report.unwrap();
            assert_eq!(report.status, expected, "{request}");
            assert_eq!(report.entries_pending, 1, "{request}");
            assert_eq!(fs::read(&source).unwrap(), b"stopped move");
            assert_eq!(fs::read(dest.join("move")).unwrap(), b"stopped move");
            client.shutdown();
            wait(&client, |s| s.closed);
            drop(client);
            {
                let journal = Journal::open(&path).unwrap();
                assert_eq!(journal.job(id).unwrap().status, expected, "{request}");
                assert_eq!(journal.details(id, 32, 0).unwrap().entries_pending, 1);
            }
            let client = Client::start(path).unwrap();
            wait(&client, |s| s.ready);
            client.send(Command::Resume(id)).unwrap();
            let resumed = wait(&client, |s| {
                s.active.is_none()
                    && s.history
                        .iter()
                        .any(|j| j.id == id && j.status == "completed")
            });
            assert!(resumed.errors.is_empty(), "{request}: {:?}", resumed.errors);
            assert!(!source.exists(), "{request}");
            assert_eq!(fs::read(dest.join("move")).unwrap(), b"stopped move");
            client.shutdown();
            wait(&client, |s| s.closed);
        }
    }

    #[test]
    fn explicit_resume_of_partial_move_finishes_cleanup_after_source_lock_is_released() {
        use std::os::windows::fs::OpenOptionsExt;
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("move-locked");
        let destination = root.path().join("dest");
        fs::create_dir(&destination).unwrap();
        fs::write(&source, b"verified partial move").unwrap();
        // Permit source reads and copy verification, but deny cleanup DELETE access.
        let source_lock = fs::OpenOptions::new()
            .read(true)
            .share_mode(1 | 2)
            .open(&source)
            .unwrap();
        let client = Client::start(root.path().join("jobs.sqlite")).unwrap();
        wait(&client, |s| s.ready);
        client
            .send(Command::Submit {
                source: source.clone(),
                destination: destination.clone(),
                conflict: Conflict::Error,
                reparse: ReparsePolicy::Reject,
                operation: OperationKind::Move,
                start: true,
            })
            .unwrap();
        let partial = wait(&client, |s| {
            s.active.is_none() && s.history.first().is_some_and(|j| j.status == "partial")
        });
        let id = partial.history[0].id;
        assert_eq!(partial.last_report.unwrap().entries_pending, 1);
        assert_eq!(fs::read(&source).unwrap(), b"verified partial move");
        assert_eq!(
            fs::read(destination.join("move-locked")).unwrap(),
            b"verified partial move"
        );
        drop(source_lock);
        client.send(Command::Resume(id)).unwrap();
        let completed = wait(&client, |s| {
            s.active.is_none()
                && s.history
                    .first()
                    .is_some_and(|j| j.id == id && j.status == "completed")
        });
        assert!(completed.errors.is_empty(), "{:?}", completed.errors);
        assert_eq!(completed.last_report.unwrap().entries_pending, 0);
        assert!(!source.exists());
        assert_eq!(
            fs::read(destination.join("move-locked")).unwrap(),
            b"verified partial move"
        );
        client.shutdown();
        wait(&client, |s| s.closed);
    }

    #[test]
    fn telemetry_counts_actual_file_and_ads_writes_once_and_resets_on_resume_and_recheck() {
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("telemetry.bin");
        let destination = root.path().join("dest");
        fs::create_dir(&destination).unwrap();
        let data = vec![49; 5 * 1024 * 1024 + 23];
        let alternate = vec![93; 173];
        fs::write(&source, &data).unwrap();
        fs::write(format!("{}:note", source.display()), &alternate).unwrap();
        let (ready_tx, ready_rx) = mpsc::sync_channel(1);
        let (go_tx, go_rx) = mpsc::sync_channel(1);
        let go_rx = Mutex::new(go_rx);
        let first = AtomicBool::new(true);
        let hook: ProgressHook = Arc::new(move |event| {
            if event.phase == recovery::Phase::CheckpointCommitted
                && first.swap(false, Ordering::SeqCst)
            {
                ready_tx.send(()).unwrap();
                go_rx.lock().unwrap().recv().unwrap();
            }
        });
        let client = Client::start_with_hook(root.path().join("jobs.sqlite"), Some(hook)).unwrap();
        wait(&client, |s| s.ready);
        submit(&client, source.clone(), destination.clone());
        let id = wait(&client, |s| s.queue.len() == 1).queue[0].id;
        client.send(Command::RunQueue(true)).unwrap();
        ready_rx.recv_timeout(Duration::from_secs(10)).unwrap();
        let active = client.snapshot().unwrap().active.unwrap();
        assert_eq!(
            active.telemetry.written,
            vartacopy_core::journal::CHUNK_BYTES as u64
        );
        assert_eq!(
            active.current_source.as_ref().unwrap(),
            &source.canonicalize().unwrap()
        );
        let first_run = active.telemetry.run_id;
        client.send(Command::Pause(id)).unwrap();
        wait(&client, |s| {
            s.active.as_ref().is_some_and(|a| a.request == "pause")
        });
        go_tx.send(()).unwrap();
        wait(&client, |s| {
            s.active.is_none() && s.history.iter().any(|j| j.id == id && j.status == "paused")
        });
        client.send(Command::Resume(id)).unwrap();
        let done = wait(&client, |s| {
            s.active.is_none()
                && s.history
                    .iter()
                    .any(|j| j.id == id && j.status == "completed")
        });
        let last = done.last_transfer.unwrap();
        assert!(last.telemetry.run_id > first_run);
        assert_eq!(
            last.telemetry.written,
            (data.len() + alternate.len() - vartacopy_core::journal::CHUNK_BYTES) as u64
        );
        assert_eq!(
            done.last_report.unwrap().resumed_bytes,
            vartacopy_core::journal::CHUNK_BYTES as u64
        );
        assert_eq!(fs::read(destination.join("telemetry.bin")).unwrap(), data);
        assert_eq!(
            fs::read(format!(
                "{}:note",
                destination.join("telemetry.bin").display()
            ))
            .unwrap(),
            alternate
        );
        let resumed_run = last.telemetry.run_id;
        client.send(Command::Recheck(id)).unwrap();
        let checked = wait(&client, |s| {
            s.active.is_none()
                && s.last_transfer
                    .as_ref()
                    .is_some_and(|a| a.telemetry.run_id > resumed_run)
        });
        assert_eq!(checked.last_transfer.unwrap().telemetry.written, 0);
        assert!(checked.errors.is_empty(), "{:?}", checked.errors);
        client.shutdown();
        wait(&client, |s| s.closed);
    }
}
