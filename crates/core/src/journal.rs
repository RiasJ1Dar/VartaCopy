//! Local SQLite journal. Filesystem mutations and SQL commits are reconciled, never treated as one transaction.
#[cfg(test)]
mod tests;
use crate::{
    Conflict, Error, Result,
    planner::{EntryKind, OperationPlan, Snapshot},
};
use rusqlite::{Connection, OpenFlags, params};
use serde::{Deserialize, Serialize};
use std::fs::File;
use std::io::{self, Write};
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::path::{Component, Path, PathBuf};
use std::time::Duration;
use vartacopy_platform_windows as platform;

const APPLICATION_ID: i32 = 0x56415254;
const VERSION: i32 = 2;
pub fn default_path() -> Result<PathBuf> {
    let root = std::env::var_os("LOCALAPPDATA")
        .ok_or_else(|| Error::Journal("LOCALAPPDATA is not configured".into()))?;
    let directory = PathBuf::from(root).join("VartaCopy");
    std::fs::create_dir_all(&directory)?;
    Ok(directory.join("jobs.sqlite3"))
}
fn sql_num<T: TryInto<i64>>(value: T) -> Result<i64> {
    value
        .try_into()
        .map_err(|_| Error::Journal("integer exceeds SQLite range".into()))
}
pub const CHUNK_BYTES: usize = 4 * 1024 * 1024;
const SCHEMA: &str = "
CREATE TABLE jobs(
 id INTEGER PRIMARY KEY AUTOINCREMENT,
 source BLOB NOT NULL CHECK(length(source)<=65534),
 destination BLOB NOT NULL CHECK(length(destination)<=65534),
 parent_identity BLOB NOT NULL,
 conflict TEXT NOT NULL,
 queue_position INTEGER NOT NULL DEFAULT 0,
 entry_count INTEGER NOT NULL CHECK(entry_count BETWEEN 1 AND 100000),
 status TEXT NOT NULL DEFAULT 'queued' CHECK(status IN ('queued','running','paused','interrupted','completed','partial','cancelled')),
 output_root BLOB, output_identity BLOB, last_error TEXT,
 created_at INTEGER NOT NULL DEFAULT(unixepoch())
);
CREATE TABLE entries(
 job_id INTEGER NOT NULL REFERENCES jobs(id), ordinal INTEGER NOT NULL CHECK(ordinal>=0),
 relative BLOB NOT NULL CHECK(length(relative)<=65534), kind TEXT NOT NULL,
 snapshot BLOB CHECK(length(snapshot)<=1048576),
 state TEXT NOT NULL DEFAULT 'planned' CHECK(state IN ('planned','copying','verified','publishing','committed','skipped','dir_intent','directory_ready')),
 stage BLOB, stage_identity BLOB, target BLOB, target_identity BLOB, last_error TEXT,
 PRIMARY KEY(job_id,ordinal)
);
CREATE TABLE chunks(
 job_id INTEGER NOT NULL, ordinal INTEGER NOT NULL, stream INTEGER NOT NULL CHECK(stream BETWEEN 0 AND 255),
 offset INTEGER NOT NULL CHECK(offset>=0), length INTEGER NOT NULL CHECK(length BETWEEN 1 AND 4194304),
 hash BLOB NOT NULL CHECK(length(hash)=32),
 PRIMARY KEY(job_id,ordinal,stream,offset),
 FOREIGN KEY(job_id,ordinal) REFERENCES entries(job_id,ordinal)
);
CREATE TABLE events(
 id INTEGER PRIMARY KEY AUTOINCREMENT, job_id INTEGER NOT NULL REFERENCES jobs(id),
 ordinal INTEGER, state TEXT NOT NULL, created_at INTEGER NOT NULL DEFAULT(unixepoch())
);
CREATE INDEX events_job ON events(job_id,id);
";

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum EntryState {
    Planned,
    Copying,
    Verified,
    Publishing,
    Committed,
    Skipped,
    DirIntent,
    DirectoryReady,
}
impl EntryState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Planned => "planned",
            Self::Copying => "copying",
            Self::Verified => "verified",
            Self::Publishing => "publishing",
            Self::Committed => "committed",
            Self::Skipped => "skipped",
            Self::DirIntent => "dir_intent",
            Self::DirectoryReady => "directory_ready",
        }
    }
    fn parse(value: &str) -> Result<Self> {
        match value {
            "planned" => Ok(Self::Planned),
            "copying" => Ok(Self::Copying),
            "verified" => Ok(Self::Verified),
            "publishing" => Ok(Self::Publishing),
            "committed" => Ok(Self::Committed),
            "skipped" => Ok(Self::Skipped),
            "dir_intent" => Ok(Self::DirIntent),
            "directory_ready" => Ok(Self::DirectoryReady),
            _ => Err(Error::Journal(format!("unknown entry state: {value}"))),
        }
    }
    fn allows(self, next: Self) -> bool {
        matches!(
            (self, next),
            (
                Self::Planned,
                Self::Copying | Self::Skipped | Self::DirIntent
            ) | (Self::Copying, Self::Verified | Self::Skipped)
                | (Self::Verified, Self::Publishing | Self::Skipped)
                | (Self::Publishing, Self::Committed | Self::Skipped)
                | (Self::DirIntent, Self::DirectoryReady)
                | (Self::DirectoryReady, Self::Committed)
        )
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Job {
    pub id: i64,
    #[serde(serialize_with = "crate::planner::serialize_path")]
    pub source: PathBuf,
    #[serde(serialize_with = "crate::planner::serialize_path")]
    pub destination: PathBuf,
    pub parent_identity: platform::Identity,
    pub conflict: Conflict,
    pub entry_count: usize,
    pub status: String,
    #[serde(serialize_with = "optional_path")]
    pub output_root: Option<PathBuf>,
    pub output_identity: Option<platform::Identity>,
    pub last_error: Option<String>,
}

fn optional_path<S: serde::Serializer>(
    path: &Option<PathBuf>,
    serializer: S,
) -> std::result::Result<S::Ok, S::Error> {
    path.as_ref()
        .map(|p| platform::PathRecord::from(p.as_path()))
        .serialize(serializer)
}

#[derive(Debug, Clone, Serialize)]
pub struct EntryIssue {
    pub ordinal: usize,
    #[serde(serialize_with = "crate::planner::serialize_path")]
    pub relative: PathBuf,
    pub state: EntryState,
    pub message: String,
}
#[derive(Debug, Clone, Serialize)]
pub struct JobDetails {
    pub job: Job,
    pub entries_committed: usize,
    pub entries_skipped: usize,
    pub entries_pending: usize,
    pub issues: Vec<EntryIssue>,
    pub issues_offset: usize,
    pub has_more: bool,
}

/// A bounded file-table projection; never exposes stage ownership or chunk hashes to the UI.
#[derive(Debug, Clone)]
pub struct EntrySummary {
    pub ordinal: usize,
    pub relative: PathBuf,
    pub kind: EntryKind,
    /// Total logical bytes across all file streams, including ADS; directories have no size.
    pub bytes: Option<u64>,
    pub state: EntryState,
    pub error: Option<String>,
}
#[derive(Debug, Clone)]
pub struct EntryPage {
    pub job_id: i64,
    pub offset: usize,
    pub total: usize,
    pub entries: Vec<EntrySummary>,
    pub has_more: bool,
}

#[derive(Debug)]
pub struct Entry {
    pub ordinal: usize,
    pub relative: PathBuf,
    pub kind: EntryKind,
    pub snapshot: Option<Snapshot>,
    pub state: EntryState,
    pub stage: Option<PathBuf>,
    pub stage_identity: Option<platform::Identity>,
    pub target: Option<PathBuf>,
    pub target_identity: Option<platform::Identity>,
}

#[derive(Debug, Clone)]
pub struct Chunk {
    pub offset: u64,
    pub length: usize,
    pub hash: [u8; 32],
}

pub struct Journal {
    db: Connection,
    _file: File,
    _writer: std::sync::Arc<File>,
    _parents: Vec<File>,
    path: PathBuf,
}

fn encode_path(path: &Path) -> Vec<u8> {
    path.as_os_str()
        .encode_wide()
        .flat_map(u16::to_le_bytes)
        .collect()
}
fn decode_path(bytes: Vec<u8>, relative: bool) -> Result<PathBuf> {
    if bytes.len() > 65534 || !bytes.len().is_multiple_of(2) {
        return Err(Error::Journal("invalid UTF-16 path length".into()));
    }
    let units: Vec<u16> = bytes
        .chunks_exact(2)
        .map(|b| u16::from_le_bytes([b[0], b[1]]))
        .collect();
    if units.contains(&0) {
        return Err(Error::Journal("NUL in persisted path".into()));
    }
    let path = PathBuf::from(std::ffi::OsString::from_wide(&units));
    if relative {
        if path
            .components()
            .any(|c| !matches!(c, Component::Normal(_)))
        {
            return Err(Error::Journal("unsafe relative path in journal".into()));
        }
    } else if !path.is_absolute()
        || path
            .components()
            .any(|c| matches!(c, Component::ParentDir | Component::CurDir))
    {
        return Err(Error::Journal("absolute normalized path required".into()));
    }
    crate::validate_path(&path)?;
    Ok(path)
}
fn kind_name(kind: EntryKind) -> &'static str {
    match kind {
        EntryKind::File => "file",
        EntryKind::Directory => "directory",
        EntryKind::Reparse => "reparse",
        EntryKind::Unsupported => "unsupported",
    }
}
fn parse_kind(kind: &str) -> Result<EntryKind> {
    match kind {
        "file" => Ok(EntryKind::File),
        "directory" => Ok(EntryKind::Directory),
        "reparse" => Ok(EntryKind::Reparse),
        _ => Err(Error::Journal(format!(
            "invalid persisted entry kind: {kind}"
        ))),
    }
}
fn optional_identity(bytes: Option<Vec<u8>>) -> Result<Option<platform::Identity>> {
    bytes
        .map(|b| serde_json::from_slice(&b).map_err(Error::from))
        .transpose()
}

impl Journal {
    /// Parent must already exist. Opening an unrelated or newer-schema DB never modifies it.
    pub fn open(path: &Path) -> Result<Self> {
        crate::validate_path(path)?;
        let path = std::path::absolute(path)?;
        let parents = platform::guard_parents(&path)?;
        let parent = path
            .parent()
            .ok_or_else(|| io::Error::other("journal parent missing"))?
            .canonicalize()?;
        platform::require_local_fixed_volume(&parent)?;
        let path = parent.join(
            path.file_name()
                .ok_or_else(|| io::Error::other("journal file name missing"))?,
        );
        let writer = platform::exclusive_lock(&path.with_extension("writer.lock"))?;
        if !path.try_exists()? {
            // Serialize a complete initialized DB, then publish it atomically by handle.
            // A crash cannot expose a half-initialized default journal.
            let memory = Connection::open_in_memory()?;
            memory.execute_batch(SCHEMA)?;
            memory.pragma_update(None, "application_id", APPLICATION_ID)?;
            memory.pragma_update(None, "user_version", VERSION)?;
            let image = memory.serialize("main")?;
            let mut stage = platform::Stage::create(&parent)?;
            stage.file().write_all(&image)?;
            stage.file().sync_all()?;
            stage.publish(&path)?;
        }
        let file = platform::guard_file(&path)?;
        if platform::link_count(&file)? != 1 {
            return Err(Error::Journal("journal hardlinks are refused".into()));
        }
        let db = Connection::open_with_flags(
            &path,
            OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;
        let application: i32 = db.pragma_query_value(None, "application_id", |r| r.get(0))?;
        let version: i32 = db.pragma_query_value(None, "user_version", |r| r.get(0))?;
        if application != APPLICATION_ID || !(1..=VERSION).contains(&version) {
            return Err(Error::Journal(format!(
                "unrecognized application/schema ({application}/{version}); database unchanged"
            )));
        }
        let check: String = db.query_row("PRAGMA quick_check", [], |r| r.get(0))?;
        if check != "ok" {
            return Err(Error::Journal(format!("integrity check failed: {check}")));
        }
        db.busy_timeout(Duration::from_secs(3))?;
        if version == 1 {
            // Atomic migration: old jobs retain their original FIFO order.
            db.execute_batch(
                "BEGIN IMMEDIATE;
                ALTER TABLE jobs ADD COLUMN queue_position INTEGER NOT NULL DEFAULT 0;
                UPDATE jobs SET queue_position=id;
                PRAGMA user_version=2;
                COMMIT;",
            )?;
        }
        let mode: String = db.pragma_update_and_check(None, "journal_mode", "WAL", |r| r.get(0))?;
        db.pragma_update(None, "synchronous", "FULL")?;
        db.pragma_update(None, "foreign_keys", true)?;
        db.pragma_update(None, "wal_autocheckpoint", 1000)?;
        db.pragma_update(None, "cache_size", -8192)?;
        db.pragma_update(None, "mmap_size", 0)?;
        let sync: i32 = db.pragma_query_value(None, "synchronous", |r| r.get(0))?;
        let fk: i32 = db.pragma_query_value(None, "foreign_keys", |r| r.get(0))?;
        if mode != "wal" || sync != 2 || fk != 1 {
            return Err(Error::Journal(
                "required WAL/FULL/foreign_keys not active".into(),
            ));
        }
        Ok(Self {
            db,
            _file: file,
            _writer: std::sync::Arc::new(writer),
            _parents: parents,
            path,
        })
    }
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// An in-process connection sharing this owner's exclusive process lease.
    /// Dropping the owner does not release the lease while any peer is alive.
    pub fn peer(&self) -> Result<Self> {
        let db = Connection::open_with_flags(
            &self.path,
            OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;
        db.busy_timeout(Duration::from_secs(3))?;
        db.pragma_update(None, "synchronous", "FULL")?;
        db.pragma_update(None, "foreign_keys", true)?;
        db.pragma_update(None, "cache_size", -8192)?;
        db.pragma_update(None, "mmap_size", 0)?;
        let mode: String = db.pragma_query_value(None, "journal_mode", |r| r.get(0))?;
        if mode != "wal" {
            return Err(Error::Journal("peer requires WAL".into()));
        }
        Ok(Self {
            db,
            _file: self._file.try_clone()?,
            _writer: self._writer.clone(),
            _parents: self
                ._parents
                .iter()
                .map(File::try_clone)
                .collect::<io::Result<_>>()?,
            path: self.path.clone(),
        })
    }

    /// Called once by the process owner before starting workers.
    pub fn recover_running_jobs(&mut self) -> Result<usize> {
        let tx = self.db.transaction()?;
        tx.execute("INSERT INTO events(job_id,state) SELECT id,'interrupted' FROM jobs WHERE status='running'", [])?;
        let count = tx.execute("UPDATE jobs SET status='interrupted',last_error='Previous process ended during transfer; resume verifies recorded data' WHERE status='running'", [])?;
        tx.commit()?;
        Ok(count)
    }

    /// Waiting jobs in persisted order, independently of history pagination.
    pub fn queue(&self, limit: usize, offset: usize) -> Result<Vec<Job>> {
        if limit > 256 {
            return Err(Error::Journal("queue page limit exceeds 256".into()));
        }
        let mut statement = self.db.prepare("SELECT id FROM jobs WHERE status='queued' ORDER BY queue_position,id LIMIT ?1 OFFSET ?2")?;
        let ids = statement
            .query_map(params![sql_num(limit)?, sql_num(offset)?], |r| {
                r.get::<_, i64>(0)
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        ids.into_iter().map(|id| self.job(id)).collect()
    }

    /// A waiting/paused/interrupted/cancelled job can be scheduled without changing its plan.
    pub fn schedule(&mut self, id: i64) -> Result<()> {
        let tx = self.db.transaction()?;
        let changed = tx.execute("UPDATE jobs SET status='queued',last_error=NULL WHERE id=?1 AND status IN ('queued','paused','interrupted','cancelled')", [id])?;
        if changed != 1 {
            return Err(Error::Journal(
                "only unfinished inactive jobs can be scheduled".into(),
            ));
        }
        tx.execute("INSERT INTO events(job_id,state) VALUES(?1,'queued')", [id])?;
        tx.commit()?;
        Ok(())
    }

    /// Change waiting order by one place. No live job or transfer data is modified.
    pub fn move_queued(&mut self, id: i64, earlier: bool) -> Result<()> {
        let tx = self.db.transaction()?;
        let position: i64 = tx.query_row(
            "SELECT queue_position FROM jobs WHERE id=?1 AND status='queued'",
            [id],
            |r| r.get(0),
        )?;
        let sql = if earlier {
            "SELECT id,queue_position FROM jobs WHERE status='queued' AND queue_position<?1 ORDER BY queue_position DESC LIMIT 1"
        } else {
            "SELECT id,queue_position FROM jobs WHERE status='queued' AND queue_position>?1 ORDER BY queue_position LIMIT 1"
        };
        use rusqlite::OptionalExtension;
        let other: Option<(i64, i64)> = tx
            .query_row(sql, [position], |r| Ok((r.get(0)?, r.get(1)?)))
            .optional()?;
        if let Some((other_id, other_position)) = other {
            tx.execute(
                "UPDATE jobs SET queue_position=?2 WHERE id=?1",
                params![id, other_position],
            )?;
            tx.execute(
                "UPDATE jobs SET queue_position=?2 WHERE id=?1",
                params![other_id, position],
            )?;
            tx.execute(
                "INSERT INTO events(job_id,state) VALUES(?1,'queue_reordered')",
                [id],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    pub fn enqueue(&mut self, plan: &OperationPlan, conflict: Conflict) -> Result<i64> {
        if !plan.is_ready() {
            return Err(Error::Journal("blocked plan cannot be enqueued".into()));
        }
        // Persist all entries atomically: a partially saved plan is never runnable.
        let tx = self.db.transaction()?;
        tx.execute("INSERT INTO jobs(source,destination,parent_identity,conflict,entry_count,queue_position) VALUES(?1,?2,?3,?4,?5,(SELECT COALESCE(MAX(queue_position),0)+1 FROM jobs))",
            params![encode_path(&plan.source),encode_path(&plan.destination),
                serde_json::to_vec(&plan.destination_parent_identity)?,serde_json::to_string(&conflict)?,sql_num(plan.entries.len())?])?;
        let id = tx.last_insert_rowid();
        {
            let mut insert=tx.prepare("INSERT INTO entries(job_id,ordinal,relative,kind,snapshot,state) VALUES(?1,?2,?3,?4,?5,?6)")?;
            for (ordinal, entry) in plan.entries.iter().enumerate() {
                let snapshot = entry
                    .snapshot
                    .as_ref()
                    .map(serde_json::to_vec)
                    .transpose()?;
                let state = if entry.kind == EntryKind::Reparse {
                    "skipped"
                } else {
                    "planned"
                };
                insert.execute(params![
                    id,
                    sql_num(ordinal)?,
                    encode_path(&entry.relative),
                    kind_name(entry.kind),
                    snapshot,
                    state
                ])?;
            }
        }
        tx.execute("INSERT INTO events(job_id,state) VALUES(?1,'queued')", [id])?;
        tx.commit()?;
        Ok(id)
    }

    pub fn job(&self, id: i64) -> Result<Job> {
        let raw=self.db.query_row("SELECT source,destination,parent_identity,conflict,entry_count,status,output_root,output_identity,last_error FROM jobs WHERE id=?1",
            [id],|r| Ok((r.get::<_,Vec<u8>>(0)?,r.get::<_,Vec<u8>>(1)?,r.get::<_,Vec<u8>>(2)?,
                r.get::<_,String>(3)?,r.get::<_,u32>(4)?,r.get::<_,String>(5)?,r.get::<_,Option<Vec<u8>>>(6)?,
                r.get::<_,Option<Vec<u8>>>(7)?,r.get::<_,Option<String>>(8)?)))?;
        if raw.4 == 0 || raw.4 as usize > crate::planner::MAX_ENTRIES {
            return Err(Error::Journal("invalid persisted entry count".into()));
        }
        Ok(Job {
            id,
            source: decode_path(raw.0, false)?,
            destination: decode_path(raw.1, false)?,
            parent_identity: serde_json::from_slice(&raw.2)?,
            conflict: serde_json::from_str(&raw.3)?,
            entry_count: raw.4 as usize,
            status: raw.5,
            output_root: raw.6.map(|b| decode_path(b, false)).transpose()?,
            output_identity: optional_identity(raw.7)?,
            last_error: raw.8,
        })
    }

    pub fn jobs(&self, limit: usize, offset: usize) -> Result<Vec<Job>> {
        if limit > 256 {
            return Err(Error::Journal("job page limit exceeds 256".into()));
        }
        let mut statement = self
            .db
            .prepare("SELECT id FROM jobs ORDER BY id DESC LIMIT ?1 OFFSET ?2")?;
        let ids = statement
            .query_map(params![sql_num(limit)?, sql_num(offset)?], |r| {
                r.get::<_, i64>(0)
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        ids.into_iter().map(|id| self.job(id)).collect()
    }

    pub fn details(&self, id: i64, limit: usize, offset: usize) -> Result<JobDetails> {
        if limit == 0 || limit > 256 {
            return Err(Error::Journal(
                "issue page must contain 1..256 entries".into(),
            ));
        }
        let job = self.job(id)?;
        let (committed, skipped) = self.counts(id)?;
        let pending = job
            .entry_count
            .checked_sub(committed + skipped)
            .ok_or_else(|| Error::Journal("invalid persisted counts".into()))?;
        let mut statement = self.db.prepare("SELECT ordinal,relative,state,last_error FROM entries WHERE job_id=?1 AND last_error IS NOT NULL ORDER BY ordinal LIMIT ?2 OFFSET ?3")?;
        let rows =
            statement.query_map(params![id, sql_num(limit + 1)?, sql_num(offset)?], |r| {
                Ok((
                    r.get::<_, u32>(0)?,
                    r.get::<_, Vec<u8>>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, String>(3)?,
                ))
            })?;
        let mut issues = Vec::new();
        for row in rows {
            let (ordinal, relative, state, message) = row?;
            issues.push(EntryIssue {
                ordinal: ordinal as usize,
                relative: decode_path(relative, true)?,
                state: EntryState::parse(&state)?,
                message,
            });
        }
        let has_more = issues.len() > limit;
        issues.truncate(limit);
        Ok(JobDetails {
            job,
            entries_committed: committed,
            entries_skipped: skipped,
            entries_pending: pending,
            issues,
            issues_offset: offset,
            has_more,
        })
    }

    pub fn entry_page(&self, id: i64, limit: usize, offset: usize) -> Result<EntryPage> {
        if limit == 0 || limit > 128 {
            return Err(Error::Journal(
                "file page must contain 1..128 entries".into(),
            ));
        }
        let total = self.job(id)?.entry_count;
        let mut statement = self.db.prepare("SELECT ordinal,relative,kind,snapshot,state,last_error FROM entries WHERE job_id=?1 ORDER BY ordinal LIMIT ?2 OFFSET ?3")?;
        let rows = statement.query_map(params![id, sql_num(limit)?, sql_num(offset)?], |r| {
            Ok((
                r.get::<_, u32>(0)?,
                r.get::<_, Vec<u8>>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, Option<Vec<u8>>>(3)?,
                r.get::<_, String>(4)?,
                r.get::<_, Option<String>>(5)?,
            ))
        })?;
        let mut entries = Vec::with_capacity(limit);
        for row in rows {
            let (ordinal, relative, kind, snapshot, state, error) = row?;
            let kind = parse_kind(&kind)?;
            let snapshot: Option<Snapshot> =
                snapshot.map(|b| serde_json::from_slice(&b)).transpose()?;
            let bytes = if kind == EntryKind::File {
                snapshot
                    .map(|s| {
                        s.streams.iter().try_fold(0u64, |sum, stream| {
                            sum.checked_add(stream.size)
                                .ok_or_else(|| Error::Journal("file stream sizes overflow".into()))
                        })
                    })
                    .transpose()?
            } else {
                None
            };
            entries.push(EntrySummary {
                ordinal: ordinal as usize,
                relative: decode_path(relative, true)?,
                kind,
                bytes,
                state: EntryState::parse(&state)?,
                error,
            });
        }
        let has_more = offset.saturating_add(entries.len()) < total;
        Ok(EntryPage {
            job_id: id,
            offset,
            total,
            entries,
            has_more,
        })
    }

    pub fn clear_entry_error(&mut self, job: i64, ordinal: usize) -> Result<()> {
        let present: bool = self.db.query_row(
            "SELECT last_error IS NOT NULL FROM entries WHERE job_id=?1 AND ordinal=?2",
            params![job, sql_num(ordinal)?],
            |r| r.get(0),
        )?;
        // Successful reconciliation normally has no error to clear: keep it read-only.
        if present {
            self.db.execute(
                "UPDATE entries SET last_error=NULL WHERE job_id=?1 AND ordinal=?2",
                params![job, sql_num(ordinal)?],
            )?;
        }
        Ok(())
    }

    /// Persistently hold or cancel an inactive unfinished job; retain owned data for recovery.
    pub fn hold(&mut self, id: i64, cancel: bool) -> Result<()> {
        let status = if cancel { "cancelled" } else { "paused" };
        let tx = self.db.transaction()?;
        if tx.execute(
            "UPDATE jobs SET status=?2 WHERE id=?1 AND status IN ('queued','paused','interrupted')",
            params![id, status],
        )? != 1
        {
            return Err(Error::Journal("job is active or already finished".into()));
        }
        tx.execute(
            "INSERT INTO events(job_id,state) VALUES(?1,?2)",
            params![id, status],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Schedule integrity reconciliation of an already finished job, without recopying it.
    pub fn recheck(&mut self, id: i64) -> Result<()> {
        let tx = self.db.transaction()?;
        if tx.execute("UPDATE jobs SET status='queued',last_error=NULL WHERE id=?1 AND status IN ('completed','partial')",[id])?!=1 {return Err(Error::Journal("only finished jobs can be rechecked".into()));}
        tx.execute(
            "INSERT INTO events(job_id,state) VALUES(?1,'recheck_queued')",
            [id],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Streaming consistent JSON snapshot, published without replacing an existing file.
    pub fn export_report(&self, id: i64, path: &Path) -> Result<()> {
        crate::validate_path(path)?;
        let path = std::path::absolute(path)?;
        let _parents = platform::guard_parents(&path)?;
        let parent = path
            .parent()
            .ok_or_else(|| Error::Journal("report parent missing".into()))?;
        let tx = self.db.unchecked_transaction()?;
        let details = self.details(id, 256, 0)?;
        let mut stage = platform::Stage::create(parent)?;
        stage
            .file()
            .write_all(b"{\"format\":\"vartacopy-job-report\",\"version\":1,\"summary\":")?;
        serde_json::to_writer(
            stage.file(),
            &serde_json::json!({
                "job":details.job,
                "entries_committed":details.entries_committed,
                "entries_skipped":details.entries_skipped,
                "entries_pending":details.entries_pending,
            }),
        )?;
        stage.file().write_all(b",\"issues\":[")?;
        let mut offset = 0;
        let mut first = true;
        loop {
            let page = self.details(id, 256, offset)?;
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

    pub fn entry(&self, job: i64, ordinal: usize) -> Result<Entry> {
        let raw=self.db.query_row("SELECT relative,kind,snapshot,state,stage,stage_identity,target,target_identity FROM entries WHERE job_id=?1 AND ordinal=?2",
            params![job,sql_num(ordinal)?],|r|Ok((r.get::<_,Vec<u8>>(0)?,r.get::<_,String>(1)?,r.get::<_,Option<Vec<u8>>>(2)?,
                r.get::<_,String>(3)?,r.get::<_,Option<Vec<u8>>>(4)?,r.get::<_,Option<Vec<u8>>>(5)?,
                r.get::<_,Option<Vec<u8>>>(6)?,r.get::<_,Option<Vec<u8>>>(7)?)))?;
        Ok(Entry {
            ordinal,
            relative: decode_path(raw.0, true)?,
            kind: parse_kind(&raw.1)?,
            snapshot: raw.2.map(|b| serde_json::from_slice(&b)).transpose()?,
            state: EntryState::parse(&raw.3)?,
            stage: raw.4.map(|b| decode_path(b, false)).transpose()?,
            stage_identity: optional_identity(raw.5)?,
            target: raw.6.map(|b| decode_path(b, false)).transpose()?,
            target_identity: optional_identity(raw.7)?,
        })
    }

    pub fn set_job_status(&mut self, job: i64, status: &str, error: Option<&str>) -> Result<()> {
        let tx = self.db.transaction()?;
        if tx.execute(
            "UPDATE jobs SET status=?2,last_error=?3 WHERE id=?1",
            params![job, status, error],
        )? != 1
        {
            return Err(Error::Journal("job does not exist".into()));
        }
        tx.execute(
            "INSERT INTO events(job_id,state) VALUES(?1,?2)",
            params![job, status],
        )?;
        tx.commit()?;
        Ok(())
    }

    pub fn set_root(&mut self, job: i64, path: &Path, identity: &platform::Identity) -> Result<()> {
        if self.db.execute(
            "UPDATE jobs SET output_root=?2,output_identity=?3 WHERE id=?1 AND output_root IS NULL",
            params![job, encode_path(path), serde_json::to_vec(identity)?],
        )? != 1
        {
            return Err(Error::Journal(
                "root already recorded or job missing".into(),
            ));
        }
        Ok(())
    }

    pub fn transition(
        &mut self,
        job: i64,
        ordinal: usize,
        from: EntryState,
        to: EntryState,
    ) -> Result<()> {
        if !from.allows(to) {
            return Err(Error::Journal(format!(
                "invalid transition {from:?} -> {to:?}"
            )));
        }
        let tx = self.db.transaction()?;
        if tx.execute("UPDATE entries SET state=?4,last_error=NULL WHERE job_id=?1 AND ordinal=?2 AND state=?3",
            params![job,sql_num(ordinal)?,from.as_str(),to.as_str()])?!=1 { return Err(Error::Journal("entry transition lost or stale".into())); }
        tx.execute(
            "INSERT INTO events(job_id,ordinal,state) VALUES(?1,?2,?3)",
            params![job, sql_num(ordinal)?, to.as_str()],
        )?;
        tx.commit()?;
        Ok(())
    }

    pub fn record_stage(
        &mut self,
        job: i64,
        ordinal: usize,
        path: &Path,
        identity: &platform::Identity,
    ) -> Result<()> {
        let tx = self.db.transaction()?;
        if tx.execute("UPDATE entries SET stage=?3,stage_identity=?4,state='copying' WHERE job_id=?1 AND ordinal=?2 AND state='planned'",
            params![job,sql_num(ordinal)?,encode_path(path),serde_json::to_vec(identity)?])?!=1 { return Err(Error::Journal("stage already recorded or entry missing".into())); }
        tx.execute(
            "INSERT INTO events(job_id,ordinal,state) VALUES(?1,?2,'copying')",
            params![job, sql_num(ordinal)?],
        )?;
        tx.commit()?;
        Ok(())
    }

    pub fn record_directory(
        &mut self,
        job: i64,
        ordinal: usize,
        stage: &Path,
        target: &Path,
        identity: &platform::Identity,
    ) -> Result<()> {
        let tx = self.db.transaction()?;
        let bytes = serde_json::to_vec(identity)?;
        if tx.execute("UPDATE entries SET stage=?3,stage_identity=?4,target=?5,state='dir_intent' WHERE job_id=?1 AND ordinal=?2 AND state='planned'",
            params![job,sql_num(ordinal)?,encode_path(stage),&bytes,encode_path(target)])?!=1 {
            return Err(Error::Journal("directory intent requires planned state".into()));
        }
        if ordinal==0 && tx.execute("UPDATE jobs SET output_root=?2,output_identity=?3 WHERE id=?1 AND output_root IS NULL",
            params![job,encode_path(target),&bytes])?!=1 {return Err(Error::Journal("root binding already exists".into()));}
        tx.execute(
            "INSERT INTO events(job_id,ordinal,state) VALUES(?1,?2,'dir_intent')",
            params![job, sql_num(ordinal)?],
        )?;
        tx.commit()?;
        Ok(())
    }

    pub fn retarget_directory(&mut self, job: i64, ordinal: usize, target: &Path) -> Result<()> {
        let tx = self.db.transaction()?;
        if tx.execute(
            "UPDATE entries SET target=?3 WHERE job_id=?1 AND ordinal=?2 AND state='dir_intent'",
            params![job, sql_num(ordinal)?, encode_path(target)],
        )? != 1
        {
            return Err(Error::Journal("directory is not publishing".into()));
        }
        if ordinal == 0 {
            tx.execute(
                "UPDATE jobs SET output_root=?2 WHERE id=?1",
                params![job, encode_path(target)],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    pub fn directory_ready(
        &mut self,
        job: i64,
        ordinal: usize,
        identity: &platform::Identity,
    ) -> Result<()> {
        let tx = self.db.transaction()?;
        if tx.execute("UPDATE entries SET state='directory_ready',target_identity=?3,last_error=NULL WHERE job_id=?1 AND ordinal=?2 AND state='dir_intent'",
            params![job,sql_num(ordinal)?,serde_json::to_vec(identity)?])?!=1 {return Err(Error::Journal("directory intent missing".into()));}
        tx.execute(
            "INSERT INTO events(job_id,ordinal,state) VALUES(?1,?2,'directory_ready')",
            params![job, sql_num(ordinal)?],
        )?;
        tx.commit()?;
        Ok(())
    }

    pub fn reopen_directory_metadata(&mut self, job: i64, ordinal: usize) -> Result<()> {
        if self.db.execute("UPDATE entries SET state='directory_ready' WHERE job_id=?1 AND ordinal=?2 AND kind='directory' AND state='committed'",
            params![job,sql_num(ordinal)?])?!=1 {return Err(Error::Journal("committed directory missing".into()));}
        Ok(())
    }

    pub fn skip_root(&mut self, job: i64) -> Result<()> {
        let tx = self.db.transaction()?;
        let active:i64=tx.query_row("SELECT count(*) FROM entries WHERE job_id=?1 AND NOT(state IN ('planned','skipped') OR (ordinal=0 AND state='dir_intent'))",[job],|r|r.get(0))?;
        if active != 0 {
            return Err(Error::Journal(
                "cannot skip a tree with work already started".into(),
            ));
        }
        tx.execute("UPDATE entries SET state='skipped' WHERE job_id=?1", [job])?;
        tx.commit()?;
        Ok(())
    }

    pub fn skip_range(&mut self, job: i64, start: usize, end: usize) -> Result<()> {
        let tx = self.db.transaction()?;
        let active:i64=tx.query_row("SELECT count(*) FROM entries WHERE job_id=?1 AND ordinal>=?2 AND ordinal<?3 AND NOT(state IN ('planned','skipped') OR (ordinal=?2 AND state='dir_intent'))",
            params![job,sql_num(start)?,sql_num(end)?],|r|r.get(0))?;
        if active != 0 {
            return Err(Error::Journal(
                "cannot skip entries with work already started".into(),
            ));
        }
        tx.execute(
            "UPDATE entries SET state='skipped' WHERE job_id=?1 AND ordinal>=?2 AND ordinal<?3",
            params![job, sql_num(start)?, sql_num(end)?],
        )?;
        tx.commit()?;
        Ok(())
    }

    pub fn entry_error(&mut self, job: i64, ordinal: usize, message: &str) -> Result<()> {
        if self.db.execute(
            "UPDATE entries SET last_error=?3 WHERE job_id=?1 AND ordinal=?2",
            params![job, sql_num(ordinal)?, message],
        )? != 1
        {
            return Err(Error::Journal("error entry missing".into()));
        }
        Ok(())
    }

    pub fn counts(&self, job: i64) -> Result<(usize, usize)> {
        let (committed,skipped)=self.db.query_row("SELECT coalesce(sum(state='committed'),0),coalesce(sum(state='skipped'),0) FROM entries WHERE job_id=?1",
            [job],|r|Ok((r.get::<_,u32>(0)?,r.get::<_,u32>(1)?)))?;
        Ok((committed as usize, skipped as usize))
    }

    pub fn publish_intent(&mut self, job: i64, ordinal: usize, target: &Path) -> Result<()> {
        let tx = self.db.transaction()?;
        if tx.execute("UPDATE entries SET target=?3,state='publishing' WHERE job_id=?1 AND ordinal=?2 AND state='verified'",
            params![job,sql_num(ordinal)?,encode_path(target)])?!=1 { return Err(Error::Journal("entry is not verified".into())); }
        tx.execute(
            "INSERT INTO events(job_id,ordinal,state) VALUES(?1,?2,'publishing')",
            params![job, sql_num(ordinal)?],
        )?;
        tx.commit()?;
        Ok(())
    }

    pub fn retarget(&mut self, job: i64, ordinal: usize, target: &Path) -> Result<()> {
        if self.db.execute(
            "UPDATE entries SET target=?3 WHERE job_id=?1 AND ordinal=?2 AND state='publishing'",
            params![job, sql_num(ordinal)?, encode_path(target)],
        )? != 1
        {
            return Err(Error::Journal("retarget requires publishing state".into()));
        }
        Ok(())
    }

    pub fn commit_file(
        &mut self,
        job: i64,
        ordinal: usize,
        identity: &platform::Identity,
    ) -> Result<()> {
        let tx = self.db.transaction()?;
        if tx.execute("UPDATE entries SET target_identity=?3,state='committed',last_error=NULL WHERE job_id=?1 AND ordinal=?2 AND state='publishing'",
            params![job,sql_num(ordinal)?,serde_json::to_vec(identity)?])?!=1 { return Err(Error::Journal("file is not publishing".into())); }
        tx.execute(
            "INSERT INTO events(job_id,ordinal,state) VALUES(?1,?2,'committed')",
            params![job, sql_num(ordinal)?],
        )?;
        tx.commit()?;
        Ok(())
    }

    pub fn checkpoint(
        &mut self,
        job: i64,
        ordinal: usize,
        stream: usize,
        chunk: &Chunk,
    ) -> Result<()> {
        let tx = self.db.transaction()?;
        let state: String = tx.query_row(
            "SELECT state FROM entries WHERE job_id=?1 AND ordinal=?2",
            params![job, sql_num(ordinal)?],
            |r| r.get(0),
        )?;
        if state != "copying" {
            return Err(Error::Journal("checkpoint requires copying state".into()));
        }
        let end:i64=tx.query_row("SELECT coalesce(max(offset+length),0) FROM chunks WHERE job_id=?1 AND ordinal=?2 AND stream=?3",
            params![job,sql_num(ordinal)?,sql_num(stream)?],|r|r.get(0))?;
        if u64::try_from(end).map_err(|_| Error::Journal("negative checkpoint end".into()))?
            != chunk.offset
            || chunk.length == 0
            || chunk.length > CHUNK_BYTES
        {
            return Err(Error::Journal("noncontiguous or invalid checkpoint".into()));
        }
        tx.execute("INSERT INTO chunks(job_id,ordinal,stream,offset,length,hash) VALUES(?1,?2,?3,?4,?5,?6)",
            params![job,sql_num(ordinal)?,sql_num(stream)?,sql_num(chunk.offset)?,sql_num(chunk.length)?,chunk.hash.as_slice()])?;
        tx.commit()?;
        Ok(())
    }

    pub fn chunks(&self, job: i64, ordinal: usize, stream: usize) -> Result<Vec<Chunk>> {
        let mut query=self.db.prepare("SELECT offset,length,hash FROM chunks WHERE job_id=?1 AND ordinal=?2 AND stream=?3 ORDER BY offset")?;
        let rows = query.query_map(params![job, sql_num(ordinal)?, sql_num(stream)?], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, u32>(1)?,
                r.get::<_, Vec<u8>>(2)?,
            ))
        })?;
        let mut chunks = Vec::new();
        let mut offset = 0u64;
        for raw in rows {
            let (start, length, hash) = raw?;
            let start =
                u64::try_from(start).map_err(|_| Error::Journal("negative chunk offset".into()))?;
            let length = length as usize;
            if chunks.len() >= 1_000_000
                || start != offset
                || length == 0
                || length > CHUNK_BYTES
                || hash.len() != 32
            {
                return Err(Error::Journal(
                    "corrupt or excessive checkpoint chain".into(),
                ));
            }
            offset = offset
                .checked_add(length as u64)
                .ok_or_else(|| Error::Journal("checkpoint overflow".into()))?;
            chunks.push(Chunk {
                offset: start,
                length,
                hash: hash
                    .try_into()
                    .map_err(|_| Error::Journal("invalid hash".into()))?,
            });
        }
        Ok(chunks)
    }

    pub fn flush(&self) -> Result<()> {
        // End a fresh WAL connection's schema-read transaction before checkpointing.
        self.db.execute_batch("BEGIN IMMEDIATE; COMMIT;")?;
        // FULL commits already synced the WAL. A peer may have a live read snapshot:
        // truncating here would falsely fail a durable completed/paused transfer.
        // The last connection checkpoints once peers have released their lease.
        if std::sync::Arc::strong_count(&self._writer) > 1 {
            return Ok(());
        }
        let busy: i32 = self
            .db
            .query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |r| r.get(0))?;
        if busy != 0 {
            return Err(Error::Journal("WAL checkpoint is busy".into()));
        }
        Ok(())
    }
}
