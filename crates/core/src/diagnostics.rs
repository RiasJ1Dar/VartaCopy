//! Local, bounded beta diagnostics. Never used as transfer/recovery authority.
mod sink;
#[cfg(test)]
mod tests;
use serde_json::{Value, json};
use std::{
    cell::RefCell,
    io,
    marker::PhantomData,
    os::windows::ffi::OsStrExt,
    path::{Path, PathBuf},
    rc::Rc,
    sync::{
        Arc, Mutex, Weak,
        atomic::{AtomicBool, Ordering},
    },
    time::{Instant, SystemTime, UNIX_EPOCH},
};
static PANIC_HOOK: AtomicBool = AtomicBool::new(false);
static PANIC_SESSION: Mutex<Option<Weak<Session>>> = Mutex::new(None);

thread_local! { static CURRENT: RefCell<Option<Arc<Session>>> = const { RefCell::new(None) }; }
pub struct Session {
    sink: Mutex<sink::Sink>,
    id: String,
    path: PathBuf,
    role: &'static str,
    started: Instant,
}
pub struct Scope {
    previous: Option<Arc<Session>>,
    _thread: PhantomData<Rc<()>>,
}
impl Drop for Scope {
    fn drop(&mut self) {
        CURRENT.with(|s| *s.borrow_mut() = self.previous.take());
    }
}
pub fn current() -> Option<Arc<Session>> {
    CURRENT.with(|s| s.borrow().clone())
}
pub fn enter(session: Option<Arc<Session>>) -> Scope {
    Scope {
        previous: CURRENT.with(|s| s.replace(session)),
        _thread: PhantomData,
    }
}
pub fn path(value: &Path) -> Value {
    let units: Vec<u16> = value.as_os_str().encode_wide().collect();
    json!({"display":value.to_string_lossy().chars().take(2048).collect::<String>(),
        "utf16":units.iter().copied().take(2048).collect::<Vec<_>>(),
        "utf16_length":units.len(),"truncated":units.len()>2048})
}
pub fn paths(values: &[PathBuf]) -> Value {
    json!({"count":values.len(),"first":values.iter().take(8).map(|p|path(p)).collect::<Vec<_>>(),
        "omitted":values.len().saturating_sub(8)})
}
pub fn error_chain(error: &(dyn std::error::Error + 'static)) -> Value {
    let mut chain = Vec::new();
    let mut next = Some(error);
    for _ in 0..16 {
        let Some(error) = next else { break };
        let io = error.downcast_ref::<io::Error>();
        chain.push(
            json!({"message":error.to_string().chars().take(8192).collect::<String>(),
            "os_error":io.and_then(io::Error::raw_os_error),
            "io_kind":io.map(|e|format!("{:?}",e.kind()))}),
        );
        next = error.source();
    }
    json!({"chain":chain,"truncated":next.is_some()})
}
impl Session {
    pub fn open(journal: &Path, role: &'static str) -> io::Result<Arc<Self>> {
        let (sink, path) = sink::Sink::open(journal)?;
        let id = format!(
            "{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_err(io::Error::other)?
                .as_nanos()
        );
        let session = Arc::new(Self {
            sink: Mutex::new(sink),
            id,
            path,
            role,
            started: Instant::now(),
        });
        if PANIC_HOOK.load(Ordering::Acquire) {
            match PANIC_SESSION.lock() {
                Ok(mut slot) => *slot = Some(Arc::downgrade(&session)),
                Err(error) => eprintln!("VartaCopy panic fallback unavailable: {error}"),
            }
        }
        session.record("info", "session.start", json!({"journal":self::path(journal),
            "executable":std::env::current_exe().map(|p|self::path(&p)).unwrap_or_else(|e|json!({"error":e.to_string()})),
            "os":std::env::consts::OS,"architecture":std::env::consts::ARCH}), true);
        Ok(session)
    }
    pub fn log_path(&self) -> &Path {
        &self.path
    }
    pub fn failure(&self) -> Option<String> {
        match self.sink.lock() {
            Ok(s) => s.failure.clone(),
            Err(e) => Some(format!("Diagnostic lock poisoned: {e}")),
        }
    }
    fn envelope(&self, level: &str, event: &str, data: Value) -> Value {
        let thread = std::thread::current();
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|v| json!(v.as_millis()))
            .unwrap_or_else(|e| json!({"clock_error":e.to_string()}));
        json!({"version":1,"session":self.id,"pid":std::process::id(),
            "thread":thread.name(),"thread_id":format!("{:?}",thread.id()),"unix_ms":timestamp,"elapsed_ms":self.started.elapsed().as_millis(),"role":self.role,
            "revision":env!("VARTACOPY_BUILD_REVISION"),"source_dirty":env!("VARTACOPY_BUILD_DIRTY"),
            "application_version":env!("CARGO_PKG_VERSION"),"level":level,"event":event,"data":data})
    }
    fn record(&self, level: &str, event: &str, data: Value, durable: bool) {
        let record = self.envelope(level, event, data);
        match self.sink.lock() {
            Ok(mut sink) => sink.record(record, durable),
            Err(error) => eprintln!("VartaCopy diagnostic lock failed: {error}"),
        }
    }
}
pub fn event(event: &str, data: Value) {
    if let Some(session) = current() {
        session.record("info", event, data, false);
    }
}
pub fn durable(event: &str, data: Value) {
    if let Some(session) = current() {
        session.record("info", event, data, true);
    }
}
pub fn failure(event: &str, error: &(dyn std::error::Error + 'static), data: Value) {
    if let Some(session) = current() {
        session.record(
            "error",
            event,
            json!({"context":data,"error":error_chain(error)}),
            true,
        );
    }
}
pub(crate) fn volume(role: &str, file: &std::fs::File, file_path: &Path) {
    if current().is_none() {
        return;
    }
    match vartacopy_platform_windows::volume_information(file) {
        Ok(info) => event(
            "volume.information",
            json!({"role":role,"path":path(file_path),"volume":info}),
        ),
        Err(error) => failure(
            "volume.information_error",
            &error,
            json!({"role":role,"path":path(file_path)}),
        ),
    }
}
/// Keep the original error and cancellation semantics; only attach diagnostic evidence.
pub(crate) fn io<T>(api: &str, file: &Path, result: io::Result<T>) -> io::Result<T> {
    if let Err(error) = &result {
        failure(
            "filesystem.error",
            error,
            json!({"api":api,"path":path(file)}),
        );
    }
    result
}
pub fn install_panic_hook() {
    PANIC_HOOK.store(true, Ordering::Release);
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let message = info
            .payload()
            .downcast_ref::<&str>()
            .copied()
            .or_else(|| info.payload().downcast_ref::<String>().map(String::as_str))
            .unwrap_or("non-string panic");
        let data = json!({"message":message.chars().take(8192).collect::<String>(),
            "location":info.location().map(ToString::to_string),
            "backtrace":std::backtrace::Backtrace::force_capture().to_string().chars().take(32768).collect::<String>()});
        let session = CURRENT
            .with(|slot| match slot.try_borrow() {
                Ok(slot) => slot.clone(),
                Err(error) => {
                    eprintln!("VartaCopy panic scope unavailable: {error}");
                    None
                }
            })
            .or_else(|| match PANIC_SESSION.try_lock() {
                Ok(slot) => slot.as_ref().and_then(Weak::upgrade),
                Err(error) => {
                    eprintln!("VartaCopy panic fallback unavailable: {error}");
                    None
                }
            });
        if let Some(session) = session {
            // A panic may occur while the sink is locked: never deadlock its hook.
            match session.sink.try_lock() {
                Ok(mut sink) => sink.record(session.envelope("error", "panic", data), true),
                Err(error) => {
                    eprintln!("VartaCopy panic diagnostic unavailable: {error}; {data}")
                }
            }
        }
        previous(info);
    }));
}
