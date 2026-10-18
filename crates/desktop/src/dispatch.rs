//! Choice-to-journal boundary. Replays never scan, enqueue or execute again.
use std::sync::atomic::{AtomicBool, Ordering};
use vartacopy_core::{
    Conflict, Error, Result,
    journal::{DispatchEngine, Journal, OperationKind},
    planner::{self, ReparsePolicy, ScanOptions},
};
use vartacopy_shell_windows::{
    native,
    transfer::{TransferOperation, TransferRequest, encode_transfer},
};

pub(crate) fn run(
    journal: &mut Journal,
    request: &TransferRequest,
    engine: DispatchEngine,
    conflict: Conflict,
    cancel: &AtomicBool,
) -> Result<(String, bool)> {
    run_with(journal, request, engine, conflict, cancel, native::execute)
}
fn run_with(
    journal: &mut Journal,
    request: &TransferRequest,
    engine: DispatchEngine,
    conflict: Conflict,
    cancel: &AtomicBool,
    native: impl FnOnce(&TransferRequest) -> std::io::Result<native::NativeResult>,
) -> Result<(String, bool)> {
    let payload = encode_transfer(request)?;
    let operation = match request.operation {
        TransferOperation::Copy => OperationKind::Copy,
        TransferOperation::Move => OperationKind::Move,
    };
    if journal.dispatch_receipt(request.id)?.is_some() {
        let receipt = journal
            .accept_dispatch(request.id, &payload, operation, conflict, Some(engine), &[])?
            .ok_or_else(|| Error::Journal("dispatch replay missing receipt".into()))?;
        return Ok((
            format!(
                "Запит уже прийнято: {:?}. Повторного запуску не було.",
                receipt.state
            ),
            false,
        ));
    }
    if cancel.load(Ordering::Acquire) {
        return Err(Error::Journal(
            "dispatch cancelled before acceptance".into(),
        ));
    }
    if engine == DispatchEngine::VartaCopy {
        let mut plans = Vec::with_capacity(request.sources.len());
        let mut remaining = planner::ScanLimits::default();
        for source in &request.sources {
            let plan = planner::scan(
                source,
                &request.destination,
                ScanOptions {
                    reparse: ReparsePolicy::Reject,
                    limits: remaining,
                },
                cancel,
            )?;
            remaining.max_entries = remaining.max_entries.saturating_sub(plan.entries().len());
            remaining.max_plan_bytes = remaining
                .max_plan_bytes
                .saturating_sub(plan.accounted_plan_bytes);
            if !plan.is_ready() {
                return Err(Error::Journal(
                    "Preflight blocked; no dispatch jobs accepted".into(),
                ));
            }
            plans.push(plan);
        }
        if cancel.load(Ordering::Acquire) {
            return Err(Error::Journal(
                "dispatch cancelled before acceptance".into(),
            ));
        }
        journal.accept_dispatch(
            request.id,
            &payload,
            operation,
            conflict,
            Some(engine),
            &plans,
        )?;
        return Ok(("Запит VartaCopy збережено в черзі".into(), true));
    }
    journal.accept_dispatch(request.id, &payload, operation, conflict, Some(engine), &[])?;
    journal.begin_native_dispatch(request.id)?;
    // From this point any panic/lost acknowledgement is an ambiguous durable intent.
    let (result, notice) = match native(request) {
        Ok(result) => {
            let notice = if result.reported_complete() {
                "Windows повідомив про завершення операції. Незалежна перевірка копії не виконувалась."
            } else {
                "Windows завершив спробу зі скасуванням або статусами, що потребують перевірки. Перевірте файли перед повтором."
            };
            (serde_json::to_vec(&result)?, notice.to_string())
        }
        Err(error) => (
            serde_json::to_vec(
                &serde_json::json!({"error":error.to_string().chars().take(4096).collect::<String>(),"filesystem_status":"unknown"}),
            )?,
            format!("Спроба Windows: {error}. Автоматичного повтору не буде."),
        ),
    };
    journal.finish_native_dispatch(request.id, &result)?;
    Ok((notice, false))
}

#[cfg(test)]
mod tests {
    use super::*;
    use vartacopy_shell_windows::transfer::TransferOrigin;
    #[test]
    fn choice_acceptance_replay_and_native_intent_are_connected_without_hidden_execution() {
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("дані.txt");
        let destination = root.path().join("target");
        std::fs::write(&source, b"choice fixture").unwrap();
        std::fs::create_dir(&destination).unwrap();
        let mut journal = Journal::open(&root.path().join("journal.sqlite")).unwrap();
        let mut request = TransferRequest {
            id: [1; 16],
            operation: TransferOperation::Copy,
            origin: TransferOrigin::AppPaste,
            sources: vec![source.clone()],
            destination: destination.clone(),
        };
        let cancel = AtomicBool::new(false);
        assert!(
            run_with(
                &mut journal,
                &request,
                DispatchEngine::VartaCopy,
                Conflict::Error,
                &cancel,
                |_| panic!("wrong engine")
            )
            .unwrap()
            .1
        );
        std::fs::remove_file(&source).unwrap(); // Replay must not rescan this missing source.
        assert!(
            !run_with(
                &mut journal,
                &request,
                DispatchEngine::VartaCopy,
                Conflict::Error,
                &cancel,
                |_| panic!("replay")
            )
            .unwrap()
            .1
        );
        assert_eq!(journal.queue(32, 0).unwrap().len(), 1);
        assert!(!destination.join("дані.txt").exists()); // Acceptance alone never copies.
        assert!(
            run_with(
                &mut journal,
                &request,
                DispatchEngine::Explorer,
                Conflict::Error,
                &cancel,
                |_| panic!("changed engine")
            )
            .is_err()
        );
        request.id = [2; 16];
        let probe = journal.peer().unwrap();
        run_with(
            &mut journal,
            &request,
            DispatchEngine::Explorer,
            Conflict::Error,
            &cancel,
            |r| {
                assert_eq!(
                    probe.dispatch_receipt(r.id).unwrap().unwrap().state,
                    vartacopy_core::journal::DispatchState::NativeIntent
                );
                Err(std::io::Error::other("fixture adapter failure"))
            },
        )
        .unwrap();
        assert_eq!(
            journal.dispatch_receipt(request.id).unwrap().unwrap().state,
            vartacopy_core::journal::DispatchState::NativeFinished
        );
        run_with(
            &mut journal,
            &request,
            DispatchEngine::Explorer,
            Conflict::Error,
            &cancel,
            |_| panic!("native replay"),
        )
        .unwrap();
        assert_eq!(journal.queue(32, 0).unwrap().len(), 1);
    }
}
