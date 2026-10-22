//! Bounded, local evidence for a transfer which failed before its result was confirmed.
//! Never performs, retries or falls back to a filesystem operation.
use std::{
    fs,
    io::{self, Read, Write},
    os::windows::ffi::OsStrExt,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};
use vartacopy_core::{
    Error,
    journal::{DispatchEngine, Journal},
};
use vartacopy_shell_windows::transfer::{TransferOperation, TransferRequest, encode_transfer};
const FORMAT: &str = "vartacopy.transfer-error";
const LIMIT: usize = 2 * 1024 * 1024;

/// Windows retry requires a known capability failure and proof no request was accepted.
/// A plain error 87 or an unreadable receipt is never sufficient.
pub(crate) fn windows_retry(
    journal: &Journal,
    request: &TransferRequest,
    engine: DispatchEngine,
    error: &Error,
) -> bool {
    if engine != DispatchEngine::VartaCopy
        || !matches!(journal.dispatch_receipt(request.id), Ok(None))
    {
        return false;
    }
    let mut next: Option<&(dyn std::error::Error + 'static)> = Some(error);
    for _ in 0..16 {
        let Some(current) = next else { break };
        if let Some(io) = current.downcast_ref::<io::Error>()
            && vartacopy_platform_windows::unsupported_identity_filesystem(io).is_some()
        {
            return true;
        }
        next = current.source();
    }
    false
}
fn units(path: &Path) -> Vec<u16> {
    path.as_os_str().encode_wide().collect()
}
fn os_code(error: &(dyn std::error::Error + 'static)) -> Option<i32> {
    let mut next = Some(error);
    for _ in 0..16 {
        let current = next?;
        if let Some(error) = current.downcast_ref::<io::Error>()
            && error.raw_os_error().is_some()
        {
            return error.raw_os_error();
        }
        next = current.source();
    }
    None
}
fn location(journal: &Path) -> io::Result<PathBuf> {
    let mut name = journal
        .file_name()
        .ok_or_else(|| io::Error::other("Journal has no filename"))?
        .to_os_string();
    name.push(".last-transfer-error.json");
    Ok(journal.with_file_name(name))
}
fn save(
    journal: &Path,
    request: &TransferRequest,
    engine: DispatchEngine,
    acceptance: &str,
    receipt_error: Option<&str>,
    error: &Error,
) -> io::Result<PathBuf> {
    let journal = journal.canonicalize()?;
    let path = location(&journal)?;
    match fs::symlink_metadata(&path) {
        Ok(metadata) => {
            use std::os::windows::fs::MetadataExt;
            if !metadata.is_file()
                || metadata.file_attributes() & 0x400 != 0
                || metadata.len() as usize > LIMIT
            {
                return Err(io::Error::other(
                    "Diagnostic path is linked, oversized or not a file",
                ));
            }
            let mut bytes = Vec::new();
            fs::File::open(&path)?
                .take((LIMIT + 1) as u64)
                .read_to_end(&mut bytes)?;
            if bytes.len() > LIMIT {
                return Err(io::Error::other("Previous diagnostic exceeds its limit"));
            }
            let previous: serde_json::Value = serde_json::from_slice(&bytes)?;
            if previous["format"] != FORMAT
                || previous["version"] != 1
                || previous["journal_utf16"] != serde_json::json!(units(&journal))
            {
                return Err(io::Error::other(
                    "Refusing to replace an unowned diagnostic",
                ));
            }
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    let payload: serde_json::Value = serde_json::from_slice(&encode_transfer(request)?)?;
    let report = serde_json::json!({"format":FORMAT,"version":1,"application_version":env!("CARGO_PKG_VERSION"),
        "created_unix":SystemTime::now().duration_since(UNIX_EPOCH).map_err(io::Error::other)?.as_secs(),
        "journal_utf16":units(&journal),"engine":format!("{engine:?}"),"acceptance":acceptance,
        "receipt_error":receipt_error,"os_error":os_code(error),
        "error":error.to_string().chars().take(8192).collect::<String>(),"request":payload});
    let bytes = serde_json::to_vec(&report)?;
    if bytes.len() > LIMIT {
        return Err(io::Error::other("Diagnostic exceeds its limit"));
    }
    let mut staged = tempfile::NamedTempFile::new_in(
        journal
            .parent()
            .ok_or_else(|| io::Error::other("Journal has no directory"))?,
    )?;
    staged.write_all(&bytes)?;
    staged.as_file().sync_all()?;
    staged.persist(&path).map_err(|e| e.error)?;
    Ok(path)
}
pub(crate) fn describe(
    journal: &Journal,
    path: &Path,
    request: &TransferRequest,
    engine: DispatchEngine,
    error: Error,
) -> Error {
    vartacopy_core::diagnostics::failure(
        "dispatch.failed",
        &error,
        serde_json::json!({
        "request_id":request.id,"engine":engine,"operation":request.operation,
        "sources":vartacopy_core::diagnostics::paths(&request.sources),
        "destination":vartacopy_core::diagnostics::path(&request.destination)}),
    );
    let (acceptance, receipt_error) = match journal.dispatch_receipt(request.id) {
        Ok(None) => ("not_accepted", None),
        Ok(Some(_)) => ("accepted", None),
        Err(error) => ("unknown", Some(error.to_string())),
    };
    let operation = match request.operation {
        TransferOperation::Copy => "Копіювання",
        TransferOperation::Move => "Переміщення",
    };
    let phase = if engine == DispatchEngine::VartaCopy && acceptance == "not_accepted" {
        "не розпочато; запит не додано до черги, видалення джерела не запускалося"
    } else {
        "не підтверджене; перевірте стан у журналі перед повтором"
    };
    let diagnostic = match save(
        path,
        request,
        engine,
        acceptance,
        receipt_error.as_deref(),
        &error,
    ) {
        Ok(path) => format!("Звіт: {}", path.display()),
        Err(report_error) => format!("Не вдалося зберегти діагностику: {report_error}"),
    };
    let receipt_context = receipt_error.map_or_else(String::new, |error| {
        format!(" Не вдалося перевірити прийняття запиту в журналі: {error}.")
    });
    Error::Journal(format!(
        "{operation} {phase}. Джерел: {}; призначення: {}. Причина: {error}.{receipt_context} {diagnostic}",
        request.sources.len(),
        request.destination.display()
    ))
}
#[cfg(test)]
mod tests {
    use super::*;
    use vartacopy_shell_windows::transfer::{TransferOrigin, TransferRequest};
    #[test]
    fn accepted_or_unreadable_receipt_never_claims_no_operation_started() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("journal.sqlite");
        let mut journal = Journal::open(&path).unwrap();
        let mut request = TransferRequest {
            id: [81; 16],
            operation: TransferOperation::Move,
            origin: TransferOrigin::Explorer,
            sources: vec![root.path().join("source")],
            destination: root.path().to_path_buf(),
        };
        journal
            .accept_dispatch(
                request.id,
                &encode_transfer(&request).unwrap(),
                vartacopy_core::journal::OperationKind::Move,
                vartacopy_core::Conflict::Error,
                Some(DispatchEngine::Explorer),
                &[],
            )
            .unwrap();
        for engine in [DispatchEngine::Explorer, DispatchEngine::VartaCopy] {
            assert!(!windows_retry(
                &journal,
                &request,
                engine,
                &io::Error::from_raw_os_error(87).into()
            ));
            let message = describe(
                &journal,
                &path,
                &request,
                engine,
                io::Error::from_raw_os_error(87).into(),
            )
            .to_string();
            assert!(message.contains("не підтверджене"));
            assert!(!message.contains("не розпочато"));
        }
        request.id = [0; 16]; // Receipt lookup fails validation: it is not evidence of absence.
        let message = describe(
            &journal,
            &path,
            &request,
            DispatchEngine::VartaCopy,
            io::Error::from_raw_os_error(87).into(),
        )
        .to_string();
        assert!(message.contains("не підтверджене"));
        assert!(message.contains("Не вдалося перевірити прийняття запиту"));
        assert!(message.contains("dispatch request ID must be nonzero"));
        assert!(!message.contains("не розпочато"));
    }
    #[test]
    fn rejected_error_87_retains_native_request_after_restart_and_preserves_unowned_files() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("journal.sqlite");
        let journal = Journal::open(&path).unwrap();
        let source = root.path().join("дані-😀.txt");
        fs::write(&source, b"keep source").unwrap();
        let request = TransferRequest {
            id: [47; 16],
            operation: TransferOperation::Move,
            origin: TransferOrigin::Explorer,
            sources: vec![source.clone()],
            destination: root.path().to_path_buf(),
        };
        let error = describe(
            &journal,
            &path,
            &request,
            DispatchEngine::VartaCopy,
            io::Error::from_raw_os_error(87).into(),
        );
        assert!(error.to_string().contains("не розпочато"));
        assert!(!windows_retry(
            &journal,
            &request,
            DispatchEngine::VartaCopy,
            &io::Error::from_raw_os_error(87).into()
        ));
        assert!(error.to_string().contains("87"));
        assert!(journal.jobs(10, 0).unwrap().is_empty());
        assert!(journal.dispatch_receipt(request.id).unwrap().is_none());
        drop(journal);
        let report_path = location(&path.canonicalize().unwrap()).unwrap();
        let report: serde_json::Value =
            serde_json::from_slice(&fs::read(&report_path).unwrap()).unwrap();
        assert_eq!(report["os_error"], 87);
        assert_eq!(report["acceptance"], "not_accepted");
        assert_eq!(
            report["request"]["sources"][0],
            serde_json::json!(units(&source))
        );
        let reopened = Journal::open(&path).unwrap();
        assert!(reopened.jobs(10, 0).unwrap().is_empty());
        assert_eq!(fs::read(&source).unwrap(), b"keep source");
        fs::write(&report_path, b"unrelated file").unwrap();
        let error = describe(
            &reopened,
            &path,
            &request,
            DispatchEngine::VartaCopy,
            io::Error::from_raw_os_error(87).into(),
        );
        assert!(
            error
                .to_string()
                .contains("Не вдалося зберегти діагностику")
        );
        assert_eq!(fs::read(&report_path).unwrap(), b"unrelated file");
    }
    #[test]
    fn repeated_reports_replace_one_owned_file_without_growth() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("journal.sqlite");
        let journal = Journal::open(&path).unwrap();
        let mut request = TransferRequest {
            id: [31; 16],
            operation: TransferOperation::Copy,
            origin: TransferOrigin::Explorer,
            sources: vec![root.path().join("source")],
            destination: root.path().to_path_buf(),
        };
        for n in 1..4 {
            request.id = [n; 16];
            describe(
                &journal,
                &path,
                &request,
                DispatchEngine::VartaCopy,
                io::Error::from_raw_os_error(87).into(),
            );
        }
        let p = location(&path.canonicalize().unwrap()).unwrap();
        let report: serde_json::Value = serde_json::from_slice(&fs::read(&p).unwrap()).unwrap();
        assert_eq!(report["request"]["id"], serde_json::json!(vec![3; 16]));
        assert!(p.metadata().unwrap().len() < LIMIT as u64);
    }
}
