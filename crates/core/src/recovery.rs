//! Resumable verified file transfers. Every reused byte is rehashed in source and stage.
#[cfg(test)]
mod tests;
use crate::{
    Conflict, Error, Result,
    control::Control,
    journal::{CHUNK_BYTES, Chunk, Entry, EntryState, Job, Journal},
    planner::EntryKind,
};
use serde::{Deserialize, Serialize};
use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use vartacopy_platform_windows as platform;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    StageRecorded,
    BlockWritten,
    BlockFlushed,
    CheckpointCommitted,
    Verified,
    PublishIntent,
    Published,
    Committed,
    Checking,
}

#[derive(Debug, Clone, Serialize)]
pub struct Progress {
    pub job_id: i64,
    pub entry: usize,
    pub phase: Phase,
    pub bytes: u64,
    pub total: u64,
}

#[derive(Debug, Serialize)]
pub struct RunReport {
    pub job_id: i64,
    pub status: String,
    pub entries_committed: usize,
    pub entries_skipped: usize,
    pub resumed_bytes: u64,
    pub verified_bytes: u64,
    pub error: Option<String>,
}
impl RunReport {
    pub fn exit_code(&self) -> u8 {
        match self.status.as_str() {
            "completed" => 0,
            "paused" | "cancelled" => 130,
            "partial" => 2,
            _ => 1,
        }
    }
}
pub type Observer<'a> = dyn FnMut(Progress) -> Result<()> + 'a;

fn notify(
    observer: &mut Observer<'_>,
    job: i64,
    entry: usize,
    phase: Phase,
    bytes: u64,
    total: u64,
) -> Result<()> {
    observer(Progress {
        job_id: job,
        entry,
        phase,
        bytes,
        total,
    })
}

fn required<T>(value: Option<T>, message: &str) -> Result<T> {
    value.ok_or_else(|| Error::Journal(message.into()))
}
fn stage_owned(entry: &Entry) -> Result<(&Path, &platform::Identity)> {
    let path = entry
        .stage
        .as_deref()
        .ok_or_else(|| Error::Journal("missing stage path".into()))?;
    let identity = entry
        .stage_identity
        .as_ref()
        .ok_or_else(|| Error::Journal("missing stage identity".into()))?;
    Ok((path, identity))
}
fn source_path(job: &Job, entry: &Entry) -> PathBuf {
    if entry.relative.as_os_str().is_empty() {
        job.source.clone()
    } else {
        job.source.join(&entry.relative)
    }
}

/// Exact contents and full checkpoint coverage, independent of source availability.
fn verify_recorded(
    journal: &Journal,
    job: i64,
    entry: &Entry,
    path: &Path,
    mut stage: Option<&mut platform::Stage>,
    control: &Control,
    observer: &mut Observer<'_>,
) -> Result<u64> {
    let snapshot = entry
        .snapshot
        .as_ref()
        .ok_or_else(|| Error::Journal("file snapshot missing".into()))?;
    let manifest = platform::streams(path)?;
    if manifest != snapshot.streams {
        return Err(Error::Verification(
            "published stream manifest differs from journal".into(),
        ));
    }
    let mut inputs = Vec::new();
    for stream in &manifest {
        let input = if let Some(stage) = stage.as_mut() {
            if stream.name.is_empty() {
                stage.file().try_clone()?
            } else {
                stage.open_stream(&stream.name)?
            }
        } else if stream.name.is_empty() {
            crate::source_file(path)?
        } else {
            platform::open_source(&platform::stream_path(path, &stream.name))?
        };
        inputs.push(input);
    }
    let mut total = 0;
    for (stream_index, stream) in manifest.iter().enumerate() {
        let input = &mut inputs[stream_index];
        input.seek(SeekFrom::Start(0))?;
        let chunks = journal.chunks(job, entry.ordinal, stream_index)?;
        let mut bytes = 0;
        let mut buffer = vec![0u8; CHUNK_BYTES];
        for chunk in chunks {
            control.checkpoint()?;
            input.read_exact(&mut buffer[..chunk.length])?;
            if blake3::hash(&buffer[..chunk.length]).as_bytes() != &chunk.hash {
                return Err(Error::Verification(
                    "recorded destination block is corrupt".into(),
                ));
            }
            bytes += chunk.length as u64;
            notify(
                observer,
                job,
                entry.ordinal,
                Phase::Checking,
                bytes,
                stream.size,
            )?;
        }
        if bytes != stream.size {
            return Err(Error::Journal("incomplete recorded stream coverage".into()));
        }
        total += bytes;
    }
    if manifest != platform::streams(path)? {
        return Err(Error::Verification(
            "stream manifest changed during recorded verification".into(),
        ));
    }
    Ok(total)
}

fn reconcile_published(
    journal: &mut Journal,
    job: &Job,
    entry: &Entry,
    control: &Control,
    observer: &mut Observer<'_>,
) -> Result<Option<u64>> {
    if !matches!(entry.state, EntryState::Publishing | EntryState::Committed) {
        return Ok(None);
    }
    let target = entry
        .target
        .as_deref()
        .ok_or_else(|| Error::Journal("publish target missing".into()))?;
    let expected = if entry.state == EntryState::Committed {
        entry.target_identity.as_ref()
    } else {
        entry.stage_identity.as_ref()
    }
    .ok_or_else(|| Error::Journal("publish identity missing".into()))?;
    let _parents = platform::guard_parents(target)?;
    match platform::open_source(target) {
        Ok(guard) => {
            if platform::identity(&guard)? != *expected {
                if entry.state == EntryState::Committed {
                    return Err(Error::Verification(
                        "committed destination was replaced".into(),
                    ));
                }
                return Ok(None);
            }
            if platform::link_count(&guard)? != 1 {
                return Err(Error::Verification("unexpected published hardlinks".into()));
            }
            let bytes = verify_recorded(journal, job.id, entry, target, None, control, observer)?;
            if entry.state == EntryState::Publishing {
                journal.commit_file(job.id, entry.ordinal, expected)?;
            }
            notify(
                observer,
                job.id,
                entry.ordinal,
                Phase::Committed,
                bytes,
                bytes,
            )?;
            Ok(Some(bytes))
        }
        Err(error)
            if error.kind() == io::ErrorKind::NotFound && entry.state == EntryState::Publishing =>
        {
            Ok(None)
        }
        Err(error) => Err(error.into()),
    }
}

/// Validate and reuse complete recorded blocks; truncate only the owned uncommitted tail.
struct StreamTask {
    job: i64,
    ordinal: usize,
    stream_index: usize,
    size: u64,
}
fn transfer_stream(
    journal: &mut Journal,
    task: StreamTask,
    input: &mut File,
    output: &mut File,
    control: &Control,
    observer: &mut Observer<'_>,
) -> Result<u64> {
    let StreamTask {
        job,
        ordinal,
        stream_index,
        size,
    } = task;
    let chunks = journal.chunks(job, ordinal, stream_index)?;
    let mut source = vec![0u8; CHUNK_BYTES];
    let mut destination = vec![0u8; CHUNK_BYTES];
    let mut offset = 0u64;
    input.seek(SeekFrom::Start(0))?;
    output.seek(SeekFrom::Start(0))?;
    for chunk in &chunks {
        control.checkpoint()?;
        if chunk.offset != offset || offset + chunk.length as u64 > size {
            return Err(Error::Journal(
                "checkpoint lies outside source stream".into(),
            ));
        }
        input.read_exact(&mut source[..chunk.length])?;
        output.read_exact(&mut destination[..chunk.length])?;
        if blake3::hash(&source[..chunk.length]).as_bytes() != &chunk.hash
            || blake3::hash(&destination[..chunk.length]).as_bytes() != &chunk.hash
        {
            return Err(Error::Verification(
                "resume prefix differs in source or stage; no bytes reused".into(),
            ));
        }
        offset += chunk.length as u64;
        notify(observer, job, ordinal, Phase::Checking, offset, size)?;
    }
    let resumed = offset;
    output.set_len(offset)?;
    while offset < size {
        control.checkpoint()?;
        let length = (size - offset).min(CHUNK_BYTES as u64) as usize;
        input.read_exact(&mut source[..length])?;
        output.write_all(&source[..length])?;
        notify(
            observer,
            job,
            ordinal,
            Phase::BlockWritten,
            offset + length as u64,
            size,
        )?;
        output.sync_all()?;
        notify(
            observer,
            job,
            ordinal,
            Phase::BlockFlushed,
            offset + length as u64,
            size,
        )?;
        journal.checkpoint(
            job,
            ordinal,
            stream_index,
            &Chunk {
                offset,
                length,
                hash: *blake3::hash(&source[..length]).as_bytes(),
            },
        )?;
        offset += length as u64;
        notify(
            observer,
            job,
            ordinal,
            Phase::CheckpointCommitted,
            offset,
            size,
        )?;
    }
    // Independent full readback. Prefix validation alone never marks the final stream verified.
    input.seek(SeekFrom::Start(0))?;
    output.seek(SeekFrom::Start(0))?;
    let mut checked = 0;
    while checked < size {
        control.checkpoint()?;
        let length = (size - checked).min(CHUNK_BYTES as u64) as usize;
        input.read_exact(&mut source[..length])?;
        output.read_exact(&mut destination[..length])?;
        if blake3::hash(&source[..length]) != blake3::hash(&destination[..length]) {
            return Err(Error::Verification(
                "full readback differs from source".into(),
            ));
        }
        checked += length as u64;
        notify(observer, job, ordinal, Phase::Checking, checked, size)?;
    }
    Ok(resumed)
}

fn resume_file(
    journal: &mut Journal,
    job: &Job,
    mut entry: Entry,
    target: &Path,
    control: &Control,
    observer: &mut Observer<'_>,
) -> Result<(bool, u64, u64)> {
    control.checkpoint()?;
    if entry.state == EntryState::Skipped {
        if let (Some(path), Some(identity)) = (&entry.stage, &entry.stage_identity) {
            if path.try_exists()? {
                let _parents = platform::guard_parents(path)?;
                let mut stage = platform::Stage::reopen(path, identity)?;
                stage.discard()?;
            }
        }
        return Ok((false, 0, 0));
    }
    if let Some(bytes) = reconcile_published(journal, job, &entry, control, observer)? {
        return Ok((true, 0, bytes));
    }
    let source = source_path(job, &entry);
    let _source_parents = platform::guard_parents(&source)?;
    let _target_parents = platform::guard_parents(target)?;
    let snapshot = entry
        .snapshot
        .as_ref()
        .ok_or_else(|| Error::Journal("file snapshot missing".into()))?;
    let main = crate::source_file(&source)?;
    snapshot.validate(&main, &source, false)?;
    if snapshot.streams.first().is_none_or(|s| !s.name.is_empty()) {
        return Err(Error::Journal("unnamed stream missing".into()));
    }
    let mut inputs = vec![main];
    for stream in snapshot.streams.iter().skip(1) {
        if stream.name.is_empty() {
            return Err(Error::Journal("duplicate unnamed stream".into()));
        }
        inputs.push(platform::open_source(&platform::stream_path(
            &source,
            &stream.name,
        ))?);
    }
    let mut stage = if entry.state == EntryState::Planned {
        let mut stage =
            platform::Stage::create(required(target.parent(), "target parent missing")?)?;
        let identity = platform::identity(stage.file())?;
        journal.record_stage(job.id, entry.ordinal, stage.path(), &identity)?;
        stage.preserve();
        notify(observer, job.id, entry.ordinal, Phase::StageRecorded, 0, 0)?;
        entry = journal.entry(job.id, entry.ordinal)?;
        stage
    } else {
        let (path, identity) = stage_owned(&entry)?;
        if *identity == snapshot.identity {
            return Err(Error::Journal("stage aliases source".into()));
        }
        if path.parent() != target.parent() {
            return Err(Error::Journal(
                "stage outside selected target directory".into(),
            ));
        }
        platform::Stage::reopen(path, identity)?
    };
    // Refresh the snapshot borrow after entry was reloaded.
    let snapshot = entry
        .snapshot
        .as_ref()
        .ok_or_else(|| Error::Journal("snapshot missing".into()))?;
    let existing = platform::streams(stage.path())?;
    if existing
        .iter()
        .any(|s| !snapshot.streams.iter().any(|source| source.name == s.name))
    {
        return Err(Error::Verification(
            "unexpected stream in staging file".into(),
        ));
    }
    let mut resumed = 0;
    if entry.state == EntryState::Copying {
        for (index, stream) in snapshot.streams.iter().enumerate() {
            let mut named;
            let output = if stream.name.is_empty() {
                stage.file()
            } else {
                named = match stage.open_stream(&stream.name) {
                    Ok(file) => file,
                    Err(error) if error.kind() == io::ErrorKind::NotFound => {
                        stage.create_stream(&stream.name)?
                    }
                    Err(error) => return Err(error.into()),
                };
                &mut named
            };
            resumed += transfer_stream(
                journal,
                StreamTask {
                    job: job.id,
                    ordinal: entry.ordinal,
                    stream_index: index,
                    size: stream.size,
                },
                &mut inputs[index],
                output,
                control,
                observer,
            )?;
        }
        snapshot.validate(&inputs[0], &source, false)?;
        journal.transition(
            job.id,
            entry.ordinal,
            EntryState::Copying,
            EntryState::Verified,
        )?;
        notify(observer, job.id, entry.ordinal, Phase::Verified, 0, 0)?;
        entry.state = EntryState::Verified;
    } else {
        // A verified stage must still match every recorded hash and the current source.
        let stage_path = stage.path().to_path_buf();
        let bytes = verify_recorded(
            journal,
            job.id,
            &entry,
            &stage_path,
            Some(&mut stage),
            control,
            observer,
        )?;
        // Compare source against hashes too, including same-size/mtime modifications.
        for (index, stream) in snapshot.streams.iter().enumerate() {
            let mut buffer = vec![0; CHUNK_BYTES];
            for chunk in journal.chunks(job.id, entry.ordinal, index)? {
                control.checkpoint()?;
                inputs[index].read_exact(&mut buffer[..chunk.length])?;
                if blake3::hash(&buffer[..chunk.length]).as_bytes() != &chunk.hash {
                    return Err(Error::Verification(
                        "source changed after verification".into(),
                    ));
                }
            }
            if inputs[index].metadata()?.len() != stream.size {
                return Err(Error::Verification("source stream length changed".into()));
            }
        }
        resumed = bytes;
    }
    snapshot.validate(&inputs[0], &source, false)?;
    platform::copy_basic_metadata(&inputs[0], stage.file())?;
    control.checkpoint()?;
    let mut candidate = entry.target.clone().unwrap_or_else(|| target.to_path_buf());
    if entry.state == EntryState::Verified {
        journal.publish_intent(job.id, entry.ordinal, &candidate)?;
    }
    for number in 0..=10_000 {
        control.checkpoint()?;
        if number > 0 {
            candidate = crate::alternative(target, number);
            journal.retarget(job.id, entry.ordinal, &candidate)?;
        }
        notify(observer, job.id, entry.ordinal, Phase::PublishIntent, 0, 0)?;
        match stage.publish(&candidate) {
            Ok(()) => {
                notify(observer, job.id, entry.ordinal, Phase::Published, 0, 0)?;
                let identity = platform::identity(stage.file())?;
                journal.commit_file(job.id, entry.ordinal, &identity)?;
                notify(observer, job.id, entry.ordinal, Phase::Committed, 0, 0)?;
                let bytes = snapshot.streams.iter().map(|s| s.size).sum();
                return Ok((true, resumed, bytes));
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => match job.conflict {
                Conflict::Error => return Err(error.into()),
                Conflict::KeepBoth => continue,
                Conflict::Skip => {
                    journal.transition(
                        job.id,
                        entry.ordinal,
                        EntryState::Publishing,
                        EntryState::Skipped,
                    )?;
                    stage.discard()?;
                    return Ok((false, resumed, 0));
                }
            },
            Err(error) => return Err(error.into()),
        }
    }
    Err(Error::Journal("keep-both candidate limit reached".into()))
}

/// Run or reconcile one persisted job. The same API is shared by CLI and GUI workers.
pub fn run(
    journal: &mut Journal,
    id: i64,
    control: &Control,
    observer: &mut Observer<'_>,
) -> Result<RunReport> {
    let job = journal.job(id)?;
    let entry = journal.entry(id, 0)?;
    if job.entry_count != 1 || entry.kind != EntryKind::File {
        return Err(Error::Journal(
            "durable tree execution is the next implementation stage".into(),
        ));
    }
    let parent = required(job.destination.parent(), "job destination parent missing")?;
    let _parents = platform::guard_parents(&parent.join(".vartacopy-guard"))?;
    let parent_guard = platform::open_directory(parent, false)?;
    if platform::identity(&parent_guard)? != job.parent_identity {
        return Err(Error::Verification("destination parent replaced".into()));
    }
    journal.set_job_status(id, "running", None)?;
    match resume_file(journal, &job, entry, &job.destination, control, observer) {
        Ok((copied, resumed, bytes)) => {
            let status = if copied { "completed" } else { "partial" };
            journal.set_job_status(id, status, None)?;
            journal.flush()?;
            Ok(RunReport {
                job_id: id,
                status: status.into(),
                entries_committed: usize::from(copied),
                entries_skipped: usize::from(!copied),
                resumed_bytes: resumed,
                verified_bytes: bytes,
                error: None,
            })
        }
        Err(error) => {
            let status = if matches!(error, Error::Cancelled) {
                "paused"
            } else {
                "interrupted"
            };
            let message = error.to_string();
            journal
                .set_job_status(id, status, Some(&message))
                .map_err(|save| {
                    Error::Journal(format!(
                        "job {id}: {message}; failed to persist error: {save}"
                    ))
                })?;
            Ok(RunReport {
                job_id: id,
                status: status.into(),
                entries_committed: 0,
                entries_skipped: 0,
                resumed_bytes: 0,
                verified_bytes: 0,
                error: Some(message),
            })
        }
    }
}
