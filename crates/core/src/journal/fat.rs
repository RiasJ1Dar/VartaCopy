//! FAT copy attempts have no persistent ownership authority. An interrupted
//! private tree is retained, never reopened for writes or automatically removed.
use super::*;
use rusqlite::OptionalExtension;

pub(super) const SCHEMA: &str = "
CREATE TABLE fat_attempts(
 id INTEGER PRIMARY KEY AUTOINCREMENT,
 job_id INTEGER NOT NULL REFERENCES jobs(id),
 stage BLOB NOT NULL CHECK(length(stage)<=65534),
 target BLOB NOT NULL CHECK(length(target)<=65534),
 state TEXT NOT NULL CHECK(state IN ('copying','publishing','committed','abandoned')),
 created_at INTEGER NOT NULL DEFAULT(unixepoch())
);
CREATE INDEX fat_attempts_job ON fat_attempts(job_id,id);
";

pub(crate) struct FatAttempt {
    pub id: i64,
    pub stage: PathBuf,
    pub target: PathBuf,
    pub state: String,
}

impl Journal {
    pub(super) fn write_fat_attempts(
        &self,
        job: i64,
        output: &mut File,
        first: &mut bool,
    ) -> Result<()> {
        let mut query = self.db.prepare(
            "SELECT id,stage,target,state FROM fat_attempts WHERE job_id=?1 ORDER BY id",
        )?;
        let rows = query.query_map([job], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, Vec<u8>>(1)?,
                r.get::<_, Vec<u8>>(2)?,
                r.get::<_, String>(3)?,
            ))
        })?;
        for row in rows {
            let (id, stage, target, state) = row?;
            if !*first {
                output.write_all(b",")?;
            }
            serde_json::to_writer(
                &mut *output,
                &serde_json::json!({"id":id,"job_id":job,
                "stage":platform::PathRecord::from(decode_path(stage,false)?.as_path()),
                "target":platform::PathRecord::from(decode_path(target,false)?.as_path()),"state":state,
                "old_stage_write_or_delete_authority":false}),
            )?;
            *first = false;
        }
        Ok(())
    }

    pub(crate) fn fat_attempt(&self, job: i64) -> Result<Option<FatAttempt>> {
        let raw = self.db.query_row(
            "SELECT id,stage,target,state FROM fat_attempts WHERE job_id=?1 ORDER BY id DESC LIMIT 1",
            [job], |r| Ok((r.get(0)?, r.get::<_, Vec<u8>>(1)?, r.get::<_, Vec<u8>>(2)?, r.get(3)?)),
        ).optional()?;
        raw.map(|(id, stage, target, state)| {
            Ok(FatAttempt {
                id,
                stage: decode_path(stage, false)?,
                target: decode_path(target, false)?,
                state,
            })
        })
        .transpose()
    }

    pub(crate) fn fat_begin(&mut self, job: i64, stage: &Path, target: &Path) -> Result<i64> {
        let saved = self.job(job)?;
        if saved.operation != OperationKind::Copy
            || target.parent() != saved.destination.parent()
            || stage.parent() != target.parent()
        {
            return Err(Error::Journal("invalid FAT copy attempt binding".into()));
        }
        let entries = (0..saved.entry_count)
            .map(|n| self.entry(job, n))
            .collect::<Result<Vec<_>>>()?;
        let tx = self.db.transaction()?;
        let committed: i64 = tx.query_row(
            "SELECT count(*) FROM fat_attempts WHERE job_id=?1 AND state='committed'",
            [job],
            |r| r.get(0),
        )?;
        if committed != 0 {
            return Err(Error::Journal(
                "completed FAT output may only be rechecked".into(),
            ));
        }
        tx.execute("UPDATE fat_attempts SET state='abandoned' WHERE job_id=?1 AND state IN ('copying','publishing')",[job])?;
        tx.execute("DELETE FROM chunks WHERE job_id=?1", [job])?;
        for entry in entries {
            let state = match entry.kind {
                EntryKind::File => "copying",
                EntryKind::Directory => "dir_intent",
                EntryKind::Reparse => "skipped",
                _ => return Err(Error::Journal("unsupported FAT entry".into())),
            };
            let output = if entry.relative.as_os_str().is_empty() {
                target.to_path_buf()
            } else {
                target.join(&entry.relative)
            };
            tx.execute("UPDATE entries SET state=?3,stage=NULL,stage_identity=NULL,target=?4,target_identity=NULL,last_error=NULL WHERE job_id=?1 AND ordinal=?2",
                params![job,sql_num(entry.ordinal)?,state,encode_path(&output)])?;
        }
        tx.execute(
            "UPDATE jobs SET output_root=?2,output_identity=NULL WHERE id=?1",
            params![job, encode_path(target)],
        )?;
        tx.execute(
            "INSERT INTO fat_attempts(job_id,stage,target,state) VALUES(?1,?2,?3,'copying')",
            params![job, encode_path(stage), encode_path(target)],
        )?;
        let id = tx.last_insert_rowid();
        tx.execute(
            "INSERT INTO events(job_id,state) VALUES(?1,'fat_fresh_attempt')",
            [job],
        )?;
        tx.commit()?;
        Ok(id)
    }

    pub(crate) fn fat_publish_intent(
        &mut self,
        job: i64,
        attempt: i64,
        target: &Path,
    ) -> Result<()> {
        let saved = self.job(job)?;
        if target.parent() != saved.destination.parent() {
            return Err(Error::Journal(
                "FAT publication outside selected directory".into(),
            ));
        }
        let entries = (0..saved.entry_count)
            .map(|n| self.entry(job, n))
            .collect::<Result<Vec<_>>>()?;
        let tx = self.db.transaction()?;
        if tx.execute("UPDATE fat_attempts SET state='publishing',target=?3 WHERE job_id=?1 AND id=?2 AND state IN ('copying','publishing')",
            params![job,attempt,encode_path(target)])?!=1 { return Err(Error::Journal("FAT attempt is not publishable".into())); }
        for entry in entries {
            let state = match (entry.kind, entry.state) {
                (EntryKind::File, EntryState::Verified | EntryState::Publishing) => "publishing",
                (EntryKind::Directory, EntryState::DirIntent | EntryState::DirectoryReady) => {
                    "directory_ready"
                }
                (EntryKind::Reparse, EntryState::Skipped) => "skipped",
                _ => {
                    return Err(Error::Journal(
                        "FAT tree contains unverified entries".into(),
                    ));
                }
            };
            let output = if entry.relative.as_os_str().is_empty() {
                target.to_path_buf()
            } else {
                target.join(&entry.relative)
            };
            tx.execute(
                "UPDATE entries SET state=?3,target=?4 WHERE job_id=?1 AND ordinal=?2",
                params![job, sql_num(entry.ordinal)?, state, encode_path(&output)],
            )?;
        }
        tx.execute(
            "UPDATE jobs SET output_root=?2 WHERE id=?1",
            params![job, encode_path(target)],
        )?;
        tx.execute(
            "INSERT INTO events(job_id,state) VALUES(?1,'fat_publish_intent')",
            [job],
        )?;
        tx.commit()?;
        Ok(())
    }

    pub(crate) fn fat_commit(
        &mut self,
        job: i64,
        attempt: i64,
        witnesses: &[(usize, platform::Identity)],
    ) -> Result<()> {
        let saved = self.job(job)?;
        let tx = self.db.transaction()?;
        if tx.execute("UPDATE fat_attempts SET state='committed' WHERE job_id=?1 AND id=?2 AND state IN ('publishing','committed')",params![job,attempt])?!=1 {
            return Err(Error::Journal("FAT publication intent missing".into()));
        }
        let required: i64 = tx.query_row(
            "SELECT count(*) FROM entries WHERE job_id=?1 AND state!='skipped'",
            [job],
            |r| r.get(0),
        )?;
        if required != sql_num(witnesses.len())? {
            return Err(Error::Journal("FAT commit coverage mismatch".into()));
        }
        for (ordinal, identity) in witnesses {
            if tx.execute("UPDATE entries SET state='committed',target_identity=?3,last_error=NULL WHERE job_id=?1 AND ordinal=?2 AND state IN ('publishing','directory_ready','committed')",
                params![job,sql_num(*ordinal)?,serde_json::to_vec(identity)?])?!=1 {return Err(Error::Journal("FAT verified entry missing".into()));}
            if *ordinal == 0 {
                tx.execute(
                    "UPDATE jobs SET output_identity=?2 WHERE id=?1",
                    params![job, serde_json::to_vec(identity)?],
                )?;
            }
        }
        tx.execute(
            "INSERT INTO events(job_id,state) VALUES(?1,'fat_committed')",
            [saved.id],
        )?;
        tx.commit()?;
        Ok(())
    }

    pub(crate) fn fat_skip(&mut self, job: i64) -> Result<()> {
        let tx = self.db.transaction()?;
        tx.execute("UPDATE fat_attempts SET state='abandoned' WHERE job_id=?1 AND state IN ('copying','publishing')",[job])?;
        tx.execute("UPDATE entries SET state='skipped' WHERE job_id=?1", [job])?;
        tx.execute(
            "INSERT INTO events(job_id,state) VALUES(?1,'fat_skipped_existing')",
            [job],
        )?;
        tx.commit()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicBool;

    #[test]
    fn schema_five_migration_preserves_jobs_and_refuses_an_unrelated_attempt_table() {
        for foreign in [false, true] {
            let root = tempfile::tempdir().unwrap();
            let source = root.path().join("source");
            let destination = root.path().join("destination");
            std::fs::write(&source, b"keep source").unwrap();
            std::fs::create_dir(&destination).unwrap();
            let plan = crate::planner::scan(
                &source,
                &destination,
                crate::planner::ScanOptions::default(),
                &AtomicBool::new(false),
            )
            .unwrap();
            let path = root.path().join("jobs.sqlite");
            let mut journal = Journal::open(&path).unwrap();
            let id = journal.enqueue(&plan, Conflict::Error).unwrap();
            let before = serde_json::to_value(journal.job(id).unwrap()).unwrap();
            journal
                .db
                .execute_batch(
                    "BEGIN IMMEDIATE; DROP TABLE fat_attempts; PRAGMA user_version=5; COMMIT;",
                )
                .unwrap();
            if foreign {
                journal.db.execute_batch("CREATE TABLE fat_attempts(keep TEXT); INSERT INTO fat_attempts VALUES('foreign data');").unwrap();
            }
            drop(journal);
            if foreign {
                assert!(Journal::open(&path).is_err());
                let db = Connection::open(&path).unwrap();
                assert_eq!(
                    db.pragma_query_value(None, "user_version", |r| r.get::<_, i32>(0))
                        .unwrap(),
                    5
                );
                assert_eq!(
                    db.query_row("SELECT keep FROM fat_attempts", [], |r| r
                        .get::<_, String>(0))
                        .unwrap(),
                    "foreign data"
                );
            } else {
                let journal = Journal::open(&path).unwrap();
                assert_eq!(
                    serde_json::to_value(journal.job(id).unwrap()).unwrap(),
                    before
                );
                assert_eq!(
                    journal
                        .db
                        .query_row("SELECT count(*) FROM entries", [], |r| r.get::<_, i64>(0))
                        .unwrap(),
                    1
                );
                assert_eq!(
                    journal
                        .db
                        .query_row("SELECT count(*) FROM fat_attempts", [], |r| r
                            .get::<_, i64>(0))
                        .unwrap(),
                    0
                );
                journal.db.pragma_update(None, "user_version", 6).unwrap();
                drop(journal);
                let journal = Journal::open(&path).unwrap();
                assert_eq!(
                    journal
                        .db
                        .pragma_query_value(None, "user_version", |r| r.get::<_, i32>(0))
                        .unwrap(),
                    VERSION
                );
                assert_eq!(
                    serde_json::to_value(journal.job(id).unwrap()).unwrap(),
                    before
                );
            }
        }
    }
}
