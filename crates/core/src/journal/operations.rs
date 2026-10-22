//! A durable user operation owns one or more independently recoverable roots.
//! Root job IDs remain stable for old reports, checkpoints and dispatch receipts.
use super::*;
#[cfg(test)]
mod tests;

pub(super) const SCHEMA: &str = "
CREATE TABLE operation_members(
 operation_id INTEGER NOT NULL REFERENCES jobs(id),
 ordinal INTEGER NOT NULL CHECK(ordinal BETWEEN 0 AND 511),
 job_id INTEGER NOT NULL UNIQUE REFERENCES jobs(id),
 PRIMARY KEY(operation_id,ordinal)
);
";
pub(super) const MIGRATE: &str = "
INSERT INTO operation_members(operation_id,ordinal,job_id)
SELECT COALESCE((SELECT first.job_id FROM dispatch_jobs d JOIN dispatch_jobs first
 ON first.receipt_id=d.receipt_id AND first.ordinal=0 WHERE d.job_id=j.id),j.id),
 COALESCE((SELECT ordinal FROM dispatch_jobs WHERE job_id=j.id),0),j.id FROM jobs j;
";

/// Older library callers could submit unrelated destinations atomically. Preserve
/// those roots as separate operations rather than inventing one target for them.
pub(super) fn split_legacy_destinations(db: &Connection) -> Result<()> {
    let mut query = db.prepare(
        "SELECT operation_id FROM operation_members GROUP BY operation_id HAVING count(*)>1",
    )?;
    let ids = query
        .query_map([], |r| r.get::<_, i64>(0))?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    for id in ids {
        let mut query=db.prepare("SELECT j.destination FROM operation_members m JOIN jobs j ON j.id=m.job_id WHERE m.operation_id=?1 ORDER BY m.ordinal LIMIT 513")?;
        let paths = query
            .query_map([id], |r| r.get::<_, Vec<u8>>(0))?
            .collect::<std::result::Result<Vec<_>, _>>()?
            .into_iter()
            .map(|b| decode_path(b, false))
            .collect::<Result<Vec<_>>>()?;
        if paths.len() > 512 {
            return Err(Error::Journal("legacy operation exceeds root limit".into()));
        }
        if paths.iter().any(|p| p.parent() != paths[0].parent()) {
            db.execute(
                "UPDATE operation_members SET operation_id=job_id,ordinal=0 WHERE operation_id=?1",
                [id],
            )?;
        }
    }
    Ok(())
}

impl Journal {
    pub fn operation_members(&self, id: i64) -> Result<Vec<Job>> {
        let leader: i64 = self.db.query_row(
            "SELECT operation_id FROM operation_members WHERE job_id=?1",
            [id],
            |r| r.get(0),
        )?;
        let mut query = self.db.prepare(
            "SELECT job_id FROM operation_members WHERE operation_id=?1 ORDER BY ordinal LIMIT 513",
        )?;
        let ids = query
            .query_map([leader], |r| r.get::<_, i64>(0))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        if ids.is_empty() || ids.len() > 512 || ids[0] != leader {
            return Err(Error::Journal(
                "invalid persisted operation membership".into(),
            ));
        }
        ids.into_iter().map(|id| self.job(id)).collect()
    }

    /// Presentation projection only. Never pass this aggregate to root recovery.
    pub fn operation(&self, id: i64) -> Result<Job> {
        let members = self.operation_members(id)?;
        let mut operation = members[0].clone();
        if members.len() == 1 {
            return Ok(operation);
        }
        let parent = operation
            .destination
            .parent()
            .ok_or_else(|| Error::Journal("operation target parent missing".into()))?
            .to_path_buf();
        if members.iter().any(|job| {
            job.operation != operation.operation
                || job.destination.parent() != Some(parent.as_path())
        }) {
            return Err(Error::Journal(
                "operation members have incompatible destinations or kinds".into(),
            ));
        }
        operation.source_count = members.len();
        operation.destination = parent.clone();
        operation.output_root = members
            .iter()
            .any(|j| j.output_root.is_some())
            .then_some(parent);
        operation.output_identity = None;
        operation.entry_count = members
            .iter()
            .try_fold(0usize, |sum, j| sum.checked_add(j.entry_count))
            .filter(|n| *n <= crate::planner::MAX_ENTRIES)
            .ok_or_else(|| Error::Journal("operation exceeds entry limit".into()))?;
        operation.status = aggregate_status(&members).into();
        operation.last_error = members.iter().find_map(|j| j.last_error.clone());
        Ok(operation)
    }

    /// Pagination counts operations, never roots hidden on another page.
    pub fn operations(&self, limit: usize, offset: usize, queued: bool) -> Result<Vec<Job>> {
        if limit > 256 {
            return Err(Error::Journal("operation page limit exceeds 256".into()));
        }
        let sql = if queued {
            "SELECT m.operation_id FROM operation_members m JOIN jobs j ON j.id=m.job_id GROUP BY m.operation_id
             HAVING SUM(j.status='queued')>0 AND SUM(j.status NOT IN ('queued','completed') AND NOT(j.status='partial' AND j.operation='copy'))=0
             ORDER BY MIN(j.queue_position),m.operation_id LIMIT ?1 OFFSET ?2"
        } else {
            "SELECT operation_id FROM operation_members GROUP BY operation_id ORDER BY MAX(job_id) DESC LIMIT ?1 OFFSET ?2"
        };
        let mut query = self.db.prepare(sql)?;
        let ids = query
            .query_map(params![sql_num(limit)?, sql_num(offset)?], |r| {
                r.get::<_, i64>(0)
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        ids.into_iter().map(|id| self.operation(id)).collect()
    }

    pub fn operation_details(&self, id: i64, limit: usize, offset: usize) -> Result<JobDetails> {
        if limit == 0 || limit > 256 {
            return Err(Error::Journal(
                "issue page must contain 1..256 entries".into(),
            ));
        }
        let job = self.operation(id)?;
        if job.source_count == 1 {
            return self.details(job.id, limit, offset);
        }
        let members = self.operation_members(job.id)?;
        let mut result = JobDetails {
            job,
            entries_committed: 0,
            entries_skipped: 0,
            entries_pending: 0,
            issues: Vec::new(),
            issues_offset: offset,
            has_more: false,
        };
        let mut skip = offset;
        let mut base = 0;
        for member in members {
            let (committed, skipped) = self.counts(member.id)?;
            result.entries_committed += committed;
            result.entries_skipped += skipped;
            let finished = if member.operation == OperationKind::Move {
                self.cleanup_removed(member.id)?
            } else {
                committed
            };
            result.entries_pending += member
                .entry_count
                .checked_sub(finished + skipped)
                .ok_or_else(|| Error::Journal("invalid operation counts".into()))?;
            let count: i64 = self.db.query_row(
                "SELECT count(*) FROM entries WHERE job_id=?1 AND last_error IS NOT NULL",
                [member.id],
                |r| r.get(0),
            )?;
            let count =
                usize::try_from(count).map_err(|_| Error::Journal("invalid issue count".into()))?;
            if skip >= count {
                skip -= count;
            } else if result.issues.len() < limit {
                let page = self.details(member.id, limit - result.issues.len(), skip)?;
                for mut issue in page.issues {
                    issue.ordinal += base;
                    issue.relative = member
                        .source
                        .file_name()
                        .map(PathBuf::from)
                        .unwrap_or_default()
                        .join(issue.relative);
                    result.issues.push(issue);
                }
                result.has_more |= page.has_more;
                skip = 0;
            } else {
                result.has_more = true;
            }
            base += member.entry_count;
        }
        Ok(result)
    }

    pub fn operation_files(&self, id: i64, limit: usize, offset: usize) -> Result<EntryPage> {
        if limit == 0 || limit > 128 {
            return Err(Error::Journal(
                "file page must contain 1..128 entries".into(),
            ));
        }
        let operation = self.operation(id)?;
        if operation.source_count == 1 {
            return self.entry_page(operation.id, limit, offset);
        }
        let mut entries = Vec::with_capacity(limit);
        let mut base = 0;
        for member in self.operation_members(operation.id)? {
            if entries.len() < limit && offset < base + member.entry_count {
                let page = self.entry_page(
                    member.id,
                    limit - entries.len(),
                    offset.saturating_sub(base),
                )?;
                for mut entry in page.entries {
                    entry.ordinal += base;
                    entry.relative = member
                        .source
                        .file_name()
                        .map(PathBuf::from)
                        .unwrap_or_default()
                        .join(entry.relative);
                    entries.push(entry);
                }
            }
            base += member.entry_count;
        }
        Ok(EntryPage {
            job_id: operation.id,
            offset,
            total: operation.entry_count,
            has_more: offset.saturating_add(entries.len()) < operation.entry_count,
            entries,
        })
    }

    pub fn schedule_operation(&mut self, id: i64) -> Result<()> {
        let members = self.operation_members(id)?;
        if members.iter().any(|j| j.status == "running") {
            return Err(Error::Journal("operation is already active".into()));
        }
        let tx = self.db.transaction()?;
        let mut changed = 0;
        for job in members {
            if job.status == "completed"
                || (job.status == "partial" && job.operation == OperationKind::Copy)
            {
                continue;
            }
            changed+=tx.execute("UPDATE jobs SET status='queued',last_error=NULL WHERE id=?1 AND status IN ('queued','paused','interrupted','cancelled','partial')",[job.id])?;
            tx.execute(
                "INSERT INTO events(job_id,state) VALUES(?1,'queued')",
                [job.id],
            )?;
        }
        if changed == 0 {
            return Err(Error::Journal(
                "only unfinished inactive operations can be scheduled".into(),
            ));
        }
        tx.commit()?;
        Ok(())
    }

    /// Cooperative transfer has already stopped before this method is called.
    /// Completed roots and closed partial Copy results retain their final state.
    pub fn hold_operation(&mut self, id: i64, cancel: bool) -> Result<()> {
        let members = self.operation_members(id)?;
        if members.iter().any(|j| j.status == "running") {
            return Err(Error::Journal("pause the active transfer first".into()));
        }
        let status = if cancel { "cancelled" } else { "paused" };
        let tx = self.db.transaction()?;
        for job in members {
            if job.status == "completed"
                || (job.status == "partial" && job.operation == OperationKind::Copy)
            {
                continue;
            }
            tx.execute(
                "UPDATE jobs SET status=?2 WHERE id=?1",
                params![job.id, status],
            )?;
            tx.execute(
                "INSERT INTO events(job_id,state) VALUES(?1,?2)",
                params![job.id, status],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    pub fn interrupt_operation(&mut self, id: i64, message: &str) -> Result<()> {
        let leader = self.operation(id)?.id;
        let tx = self.db.transaction()?;
        tx.execute("UPDATE jobs SET status='interrupted',last_error=?2 WHERE status='running' AND id IN (SELECT job_id FROM operation_members WHERE operation_id=?1)",params![leader,message])?;
        tx.commit()?;
        Ok(())
    }

    pub fn recheck_operation(&mut self, id: i64) -> Result<()> {
        let operation = self.operation(id)?;
        if operation.operation != OperationKind::Copy
            || !matches!(operation.status.as_str(), "completed" | "partial")
        {
            return Err(Error::Journal(
                "only finished Copy operations can be rechecked".into(),
            ));
        }
        let members = self.operation_members(operation.id)?;
        if members
            .iter()
            .any(|j| !matches!(j.status.as_str(), "completed" | "partial"))
        {
            return Err(Error::Journal(
                "operation still has unfinished roots".into(),
            ));
        }
        let tx = self.db.transaction()?;
        for job in members {
            tx.execute(
                "UPDATE jobs SET status='queued',last_error=NULL WHERE id=?1",
                [job.id],
            )?;
            tx.execute(
                "INSERT INTO events(job_id,state) VALUES(?1,'recheck_queued')",
                [job.id],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    pub fn move_operation(&mut self, id: i64, earlier: bool) -> Result<()> {
        let queue = self.operations(256, 0, true)?;
        let index = queue
            .iter()
            .position(|j| j.id == id)
            .ok_or_else(|| Error::Journal("waiting operation is not in queue page".into()))?;
        let other = if earlier {
            index.checked_sub(1)
        } else {
            index.checked_add(1).filter(|n| *n < queue.len())
        };
        let Some(other) = other else {
            return Ok(());
        };
        let other = queue[other].id;
        let position = |id| -> Result<i64> {
            Ok(self.db.query_row("SELECT MIN(j.queue_position) FROM jobs j JOIN operation_members m ON m.job_id=j.id WHERE m.operation_id=?1",[id],|r|r.get(0))?)
        };
        let first = position(id)?;
        let second = position(other)?;
        let tx = self.db.transaction()?;
        tx.execute("UPDATE jobs SET queue_position=?2 WHERE id IN (SELECT job_id FROM operation_members WHERE operation_id=?1)",params![id,second])?;
        tx.execute("UPDATE jobs SET queue_position=?2 WHERE id IN (SELECT job_id FROM operation_members WHERE operation_id=?1)",params![other,first])?;
        tx.execute(
            "INSERT INTO events(job_id,state) VALUES(?1,'operation_reordered')",
            [id],
        )?;
        tx.commit()?;
        Ok(())
    }

    pub fn export_operation_report(&self, id: i64, path: &Path) -> Result<()> {
        let operation = self.operation(id)?;
        if operation.source_count == 1 {
            return self.export_report(operation.id, path);
        }
        crate::validate_path(path)?;
        let path = std::path::absolute(path)?;
        let _parents = platform::guard_parents(&path)?;
        let tx = self.db.unchecked_transaction()?;
        let details = self.operation_details(operation.id, 256, 0)?;
        let mut stage = platform::Stage::create(
            path.parent()
                .ok_or_else(|| Error::Journal("report parent missing".into()))?,
        )?;
        stage
            .file()
            .write_all(b"{\"format\":\"vartacopy-operation-report\",\"version\":1,\"summary\":")?;
        serde_json::to_writer(stage.file(), &details)?;
        stage.file().write_all(b",\"members\":")?;
        serde_json::to_writer(stage.file(), &self.operation_members(operation.id)?)?;
        stage.file().write_all(b",\"issues\":[")?;
        let mut offset = 0;
        let mut first = true;
        loop {
            let page = self.operation_details(operation.id, 256, offset)?;
            for issue in page.issues {
                if !first {
                    stage.file().write_all(b",")?;
                }
                serde_json::to_writer(stage.file(), &issue)?;
                first = false;
            }
            if !page.has_more {
                break;
            }
            offset += 256;
        }
        stage.file().write_all(b"]}\n")?;
        stage.file().sync_all()?;
        tx.commit()?;
        stage.publish(&path)?;
        Ok(())
    }
}

fn aggregate_status(members: &[Job]) -> &'static str {
    for status in ["running", "paused", "interrupted", "cancelled"] {
        if members.iter().any(|j| j.status == status) {
            return status;
        }
    }
    if members
        .iter()
        .any(|j| j.status == "partial" && j.operation == OperationKind::Move)
    {
        return "partial";
    }
    if members.iter().any(|j| j.status == "queued") {
        return "queued";
    }
    if members.iter().any(|j| j.status == "partial") {
        return "partial";
    }
    "completed"
}
