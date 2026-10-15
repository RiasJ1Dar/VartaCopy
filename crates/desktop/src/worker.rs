//! Bounded desktop actor: planning and transfer never run on the UI thread.
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
    journal::{Job, JobDetails, Journal},
    planner::{self, OperationPlan, ReparsePolicy, ScanOptions},
    recovery::{self, Progress, RunReport},
};

#[derive(Debug, Clone, Default)]
pub struct State {
    pub ready: bool,
    pub closed: bool,
    pub planning: bool,
    pub automatic: bool,
    pub active: Option<Active>,
    pub queue: Vec<Job>,
    pub history: Vec<Job>,
    pub offset: usize,
    pub errors: VecDeque<String>,
    pub last_report: Option<Arc<RunReport>>,
    pub details: Option<Arc<JobDetails>>,
    pub notice: Option<String>,
}
#[derive(Debug, Clone)]
pub struct Active {
    pub id: i64,
    pub entry_count: usize,
    pub progress: Option<Progress>,
    pub request: &'static str,
}
pub enum Command {
    Submit {
        source: PathBuf,
        destination: PathBuf,
        conflict: Conflict,
        reparse: ReparsePolicy,
    },
    RunQueue(bool),
    Resume(i64),
    Pause(i64),
    Cancel(i64),
    Move {
        id: i64,
        earlier: bool,
    },
    Page(usize),
    Inspect {
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
    Wake,
    Command(Command),
    Planned(Result<OperationPlan>, Conflict),
    Finished(i64, Result<RunReport>),
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
    if let Err(failure) = edit(state, |s| {
        if s.errors.len() == 32 {
            s.errors.pop_front();
        }
        s.errors.push_back(message.clone());
    }) {
        eprintln!("VartaCopy: {message}; {failure}");
    }
}
type ProgressHook = Arc<dyn Fn(&Progress) + Send + Sync>;
impl Client {
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
                    Ok(Err(e)) => error(&state, e),
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

fn refresh(journal: &Journal, state: &Shared) -> Result<()> {
    let offset = quiet(state, |s| s.offset)?;
    let queue = journal.queue(32, 0)?;
    let history = journal.jobs(32, offset)?;
    let selected = quiet(state, |s| {
        s.details.as_ref().map(|d| (d.job.id, d.issues_offset))
    })?;
    let details = selected
        .map(|(id, offset)| journal.details(id, 32, offset).map(Arc::new))
        .transpose()?;
    edit(state, |s| {
        s.queue = queue;
        s.history = history;
        s.details = details;
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
    let mut active_id = None;
    let mut cancelled = false;
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
            if transfer.is_none() && planning.is_none() {
                break;
            }
        }
        if let Ok(message) = receiver.recv() {
            let result: Result<()> = (|| {
                match message {
                    Message::Wake => {}
                    Message::Command(Command::Submit {
                        source,
                        destination,
                        conflict,
                        reparse,
                    }) => {
                        if planning.is_some() || shutdown.load(Ordering::Acquire) {
                            return Err(Error::Journal(
                                "another scan is active or the application is closing".into(),
                            ));
                        }
                        scan_cancel.store(false, Ordering::Relaxed);
                        edit(state, |s| s.planning = true)?;
                        let tx = sender.clone();
                        let cancel = scan_cancel.clone();
                        planning =
                            Some(thread::Builder::new().name("vartacopy-scan".into()).spawn(
                                move || {
                                    let outcome = std::panic::catch_unwind(|| {
                                        planner::scan(
                                            &source,
                                            &destination,
                                            ScanOptions {
                                                reparse,
                                                ..ScanOptions::default()
                                            },
                                            &cancel,
                                        )
                                    });
                                    let result = outcome.unwrap_or_else(|_| {
                                        Err(Error::Journal("scanner panicked".into()))
                                    });
                                    if let Err(e) = tx.send(Message::Planned(result, conflict)) {
                                        eprintln!("VartaCopy: scan result undelivered: {e}");
                                    }
                                },
                            )?);
                    }
                    Message::Planned(result, conflict) => {
                        if let Some(handle) = planning.take() {
                            handle
                                .join()
                                .map_err(|_| Error::Journal("scanner thread panicked".into()))?;
                        }
                        edit(state, |s| s.planning = false)?;
                        let plan = result?;
                        if !plan.is_ready() {
                            let problems = plan
                                .entries()
                                .iter()
                                .filter_map(|entry| {
                                    entry.problem().map(|problem| {
                                        format!("{}: {problem}", entry.relative_path().display())
                                    })
                                })
                                .take(8)
                                .collect::<Vec<_>>()
                                .join("; ");
                            return Err(Error::Journal(format!("Preflight blocked: {problems}")));
                        }
                        // A queued acknowledgement is visible only after this durable transaction.
                        journal.enqueue(&plan, conflict)?;
                        refresh(&journal, state)?;
                    }
                    Message::Command(Command::Inspect { id, offset }) => {
                        let details = journal.details(id, 32, offset)?;
                        edit(state, |s| s.details = Some(Arc::new(details)))?;
                    }
                    Message::Command(Command::Export { id, path }) => {
                        if active_id == Some(id) {
                            return Err(Error::Journal(
                                "Pause this job before exporting a final report".into(),
                            ));
                        }
                        journal.export_report(id, &path)?;
                        edit(state, |s| {
                            s.notice = Some(format!("Звіт збережено: {}", path.display()))
                        })?;
                    }
                    Message::Command(Command::Recheck(id)) => {
                        journal.recheck(id)?;
                        automatic = true;
                        refresh(&journal, state)?;
                    }
                    Message::Command(Command::ClearErrors) => {
                        edit(state, |s| {
                            s.errors.clear();
                            s.notice = None;
                        })?;
                    }
                    Message::Command(Command::RunQueue(value)) => automatic = value,
                    Message::Command(Command::Resume(id)) => {
                        if active_id == Some(id) {
                            return Err(Error::Journal("job is already active".into()));
                        }
                        journal.schedule(id)?;
                        automatic = true;
                        refresh(&journal, state)?;
                    }
                    Message::Command(Command::Pause(id)) => {
                        automatic = false;
                        if active_id == Some(id) {
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
                            journal.hold(id, false)?;
                            refresh(&journal, state)?;
                        }
                    }
                    Message::Command(Command::Cancel(id)) => {
                        if active_id == Some(id) {
                            cancelled = true;
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
                            journal.hold(id, true)?;
                            refresh(&journal, state)?;
                        }
                    }
                    Message::Command(Command::Move { id, earlier }) => {
                        journal.move_queued(id, earlier)?;
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
                        edit(state, |s| s.active = None)?;
                        match result {
                            Ok(mut report) => {
                                if cancelled && report.status == "paused" {
                                    report.status = "cancelled".into();
                                    journal.set_job_status(id,"cancelled",Some("Stopped; owned partial files are retained for recovery"))?;
                                }
                                edit(state, |s| s.last_report = Some(Arc::new(report)))?;
                            }
                            Err(e) => {
                                journal.set_job_status(id, "interrupted", Some(&e.to_string()))?;
                                error(state, e);
                            }
                        }
                        cancelled = false;
                        refresh(&journal, state)?;
                    }
                }
                Ok(())
            })();
            if let Err(e) = result {
                error(state, e);
            }
        } else {
            break;
        }
        if automatic && transfer.is_none() && !shutdown.load(Ordering::Acquire) {
            if let Some(job) = journal.queue(1, 0)?.into_iter().next() {
                let mut peer = journal.peer()?;
                let control = Arc::new(Control::default());
                *active_control
                    .lock()
                    .map_err(|_| Error::Journal("transfer control poisoned".into()))? =
                    Some(control.clone());
                journal.set_job_status(job.id, "running", None)?;
                let tx = sender.clone();
                let shared = state.clone();
                let progress_hook = hook.clone();
                let id = job.id;
                active_id = Some(id);
                edit(state, |s| {
                    s.active = Some(Active {
                        id,
                        entry_count: job.entry_count,
                        progress: None,
                        request: "",
                    })
                })?;
                transfer = Some(
                    thread::Builder::new()
                        .name("vartacopy-transfer".into())
                        .spawn(move || {
                            let mut last_progress = Instant::now();
                            let outcome =
                                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                                    recovery::run(&mut peer, id, &control, &mut |event| {
                                        if let Some(hook) = progress_hook.as_ref() {
                                            hook(&event);
                                        }
                                        quiet(&shared, |s| {
                                            if let Some(a) = s.active.as_mut() {
                                                a.progress = Some(event);
                                            }
                                        })?;
                                        if last_progress.elapsed() >= Duration::from_millis(100) {
                                            (shared.wake)();
                                            last_progress = Instant::now();
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
    }
    journal.flush()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{fs, sync::atomic::AtomicUsize, time::Instant};
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
    fn submit(client: &Client, source: PathBuf, destination: PathBuf) {
        client
            .send(Command::Submit {
                source,
                destination,
                conflict: Conflict::Error,
                reparse: ReparsePolicy::Reject,
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
}
