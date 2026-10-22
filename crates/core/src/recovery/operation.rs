use super::*;

/// One user transfer, one control and monotonic entry numbering across all roots.
/// Each root retains its own publication/cleanup checkpoints and stable job ID.
pub fn run_operation(
    journal: &mut Journal,
    id: i64,
    control: &Control,
    observer: &mut Observer<'_>,
) -> Result<RunReport> {
    let operation = journal.operation(id)?;
    let members = journal.operation_members(operation.id)?;
    let reconcile_finished = operation.status == "completed";
    diagnostics::durable(
        "operation.start",
        serde_json::json!({"operation_id":operation.id,"root_job_ids":members.iter().map(|j|j.id).collect::<Vec<_>>(),"entries":operation.entry_count}),
    );
    let mut report = RunReport {
        job_id: operation.id,
        destination: operation.destination.as_path().into(),
        status: operation.status,
        entries_committed: 0,
        entries_skipped: 0,
        entries_failed: 0,
        entries_pending: 0,
        resumed_bytes: 0,
        verified_bytes: 0,
        error: None,
        issues: Vec::new(),
        issues_omitted: 0,
    };
    let mut base = 0;
    for member in members {
        if control.is_cancelled() {
            break;
        }
        if (member.status == "completed" && !reconcile_finished)
            || (member.status == "partial" && member.operation == OperationKind::Copy)
        {
            base += member.entry_count;
            continue;
        }
        if member.status != "queued" && !(reconcile_finished && member.status == "completed") {
            return Err(Error::Journal(
                "schedule the whole operation before running its roots".into(),
            ));
        }
        journal.set_job_status(member.id, "running", None)?;
        let result = run(journal, member.id, control, &mut |mut event| {
            event.job_id = operation.id;
            event.entry += base;
            observer(event)
        });
        let root = match result {
            Ok(root) => root,
            Err(Error::Cancelled) => {
                journal.set_job_status(member.id, "paused", None)?;
                break;
            }
            Err(error) => {
                journal
                    .set_job_status(member.id, "interrupted", Some(&error.to_string()))
                    .map_err(|save| {
                        Error::Journal(format!("{error}; cannot persist root failure: {save}"))
                    })?;
                return Err(error);
            }
        };
        report.entries_failed += root.entries_failed;
        report.resumed_bytes += root.resumed_bytes;
        report.verified_bytes += root.verified_bytes;
        report.error = report.error.or(root.error);
        report.issues_omitted += root.issues_omitted;
        for issue in root.issues {
            if report.issues.len() < 256 {
                report.issues.push(issue);
            } else {
                report.issues_omitted += 1;
            }
        }
        base += member.entry_count;
    }
    let details = journal.operation_details(operation.id, 1, 0)?;
    report.status = details.job.status;
    report.entries_committed = details.entries_committed;
    report.entries_skipped = details.entries_skipped;
    report.entries_pending = details.entries_pending;
    diagnostics::durable(
        "operation.finish",
        serde_json::json!({"operation_id":operation.id,"status":report.status,"committed":report.entries_committed,"pending":report.entries_pending}),
    );
    Ok(report)
}
