//! Durable deduplication records, not authorization or filesystem completion proofs.
use super::*;
use rusqlite::{OptionalExtension, TransactionBehavior};
#[cfg(test)]
mod tests;

pub const MAX_CANONICAL_REQUEST_BYTES: usize = 1024 * 1024;
pub const MAX_NATIVE_RESULT_BYTES: usize = 64 * 1024;
pub(super) const SCHEMA: &str = "
CREATE TABLE dispatch_requests(
 receipt_id INTEGER PRIMARY KEY AUTOINCREMENT,
 request_id BLOB NOT NULL UNIQUE CHECK(length(request_id)=16 AND request_id!=zeroblob(16)),
 canonical_payload BLOB NOT NULL CHECK(length(canonical_payload) BETWEEN 1 AND 1048576),
 binding_digest BLOB NOT NULL CHECK(length(binding_digest)=32),
 engine TEXT NOT NULL CHECK(engine IN ('vartacopy','explorer')),
 operation TEXT NOT NULL CHECK(operation IN ('copy','move')),
 conflict TEXT NOT NULL CHECK(conflict IN ('error','skip','keep_both')),
 state TEXT NOT NULL CHECK(state IN ('accepted','native_intent','native_finished')),
 native_result BLOB CHECK(length(native_result) BETWEEN 1 AND 65536),
 created_at INTEGER NOT NULL DEFAULT(unixepoch()),
 CHECK(engine='explorer' OR state='accepted'),
 CHECK((state='native_finished' AND native_result IS NOT NULL) OR (state!='native_finished' AND native_result IS NULL))
);
CREATE TABLE dispatch_jobs(
 receipt_id INTEGER NOT NULL REFERENCES dispatch_requests(receipt_id),
 ordinal INTEGER NOT NULL CHECK(ordinal BETWEEN 0 AND 511),
 job_id INTEGER NOT NULL UNIQUE REFERENCES jobs(id),
 PRIMARY KEY(receipt_id,ordinal)
);
CREATE INDEX dispatch_unresolved ON dispatch_requests(engine,state,receipt_id);
";

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum DispatchEngine {
    VartaCopy,
    Explorer,
}
impl DispatchEngine {
    fn name(self) -> &'static str {
        match self {
            Self::VartaCopy => "vartacopy",
            Self::Explorer => "explorer",
        }
    }
    fn parse(value: &str) -> Result<Self> {
        match value {
            "vartacopy" => Ok(Self::VartaCopy),
            "explorer" => Ok(Self::Explorer),
            _ => Err(Error::Journal("unknown dispatch engine".into())),
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum DispatchState {
    Accepted,
    /// Dispatch might have started. After restart it needs reconciliation, never automatic retry.
    NativeIntent,
    /// A caller-recorded attempt result, not proof that filesystem Copy/Move succeeded.
    NativeFinished,
}
impl DispatchState {
    fn parse(value: &str) -> Result<Self> {
        match value {
            "accepted" => Ok(Self::Accepted),
            "native_intent" => Ok(Self::NativeIntent),
            "native_finished" => Ok(Self::NativeFinished),
            _ => Err(Error::Journal("unknown dispatch state".into())),
        }
    }
}

/// A stable journal record. Possession of its ID is not authorization to dispatch.
#[derive(Debug, Clone, Serialize)]
pub struct DispatchReceipt {
    pub receipt_id: i64,
    pub request_id: [u8; 16],
    pub binding_digest: [u8; 32],
    pub engine: DispatchEngine,
    pub operation: OperationKind,
    pub conflict: Conflict,
    pub state: DispatchState,
    pub job_ids: Vec<i64>,
    /// Bounded opaque result supplied by the trusted native adapter.
    pub native_result: Option<Vec<u8>>,
}
#[derive(Debug, Clone, Serialize)]
pub struct DispatchPage {
    pub receipts: Vec<DispatchReceipt>,
    pub offset: usize,
    pub has_more: bool,
}

fn valid_id(id: &[u8; 16]) -> Result<()> {
    if *id == [0; 16] {
        return Err(Error::Journal("dispatch request ID must be nonzero".into()));
    }
    Ok(())
}
fn conflict_name(conflict: Conflict) -> &'static str {
    match conflict {
        Conflict::Error => "error",
        Conflict::Skip => "skip",
        Conflict::KeepBoth => "keep_both",
    }
}
fn parse_conflict(value: &str) -> Result<Conflict> {
    match value {
        "error" => Ok(Conflict::Error),
        "skip" => Ok(Conflict::Skip),
        "keep_both" => Ok(Conflict::KeepBoth),
        _ => Err(Error::Journal("unknown dispatch conflict policy".into())),
    }
}
fn binding(
    payload: &[u8],
    engine: DispatchEngine,
    operation: OperationKind,
    conflict: Conflict,
) -> [u8; 32] {
    let mut digest = blake3::Hasher::new_derive_key("VartaCopy dispatch request binding v1");
    for field in [
        engine.name().as_bytes(),
        operation.as_str().as_bytes(),
        conflict_name(conflict).as_bytes(),
        payload,
    ] {
        digest.update(&(field.len() as u64).to_le_bytes());
        digest.update(field);
    }
    *digest.finalize().as_bytes()
}
fn payload(db: &Connection, id: &[u8; 16]) -> Result<Option<Vec<u8>>> {
    Ok(db
        .query_row(
            "SELECT canonical_payload FROM dispatch_requests WHERE request_id=?1",
            [id.as_slice()],
            |row| row.get(0),
        )
        .optional()?)
}
fn receipt(db: &Connection, id: &[u8; 16]) -> Result<Option<DispatchReceipt>> {
    let raw=db.query_row("SELECT receipt_id,binding_digest,engine,operation,conflict,state,native_result FROM dispatch_requests WHERE request_id=?1",
        [id.as_slice()],|r|Ok((r.get::<_,i64>(0)?,r.get::<_,Vec<u8>>(1)?,r.get::<_,String>(2)?,r.get::<_,String>(3)?,r.get::<_,String>(4)?,r.get::<_,String>(5)?,r.get::<_,Option<Vec<u8>>>(6)?))).optional()?;
    let Some(raw) = raw else {
        return Ok(None);
    };
    let mut query = db.prepare(
        "SELECT ordinal,job_id FROM dispatch_jobs WHERE receipt_id=?1 ORDER BY ordinal LIMIT 513",
    )?;
    let links = query
        .query_map([raw.0], |r| Ok((r.get::<_, u32>(0)?, r.get::<_, i64>(1)?)))?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    if links.len() > 512
        || links
            .iter()
            .enumerate()
            .any(|(ordinal, (saved, _))| ordinal != *saved as usize)
    {
        return Err(Error::Journal("invalid dispatch job sequence".into()));
    }
    let engine = DispatchEngine::parse(&raw.2)?;
    if (engine == DispatchEngine::Explorer && !links.is_empty())
        || (engine == DispatchEngine::VartaCopy && links.is_empty())
    {
        return Err(Error::Journal(
            "dispatch engine/job binding mismatch".into(),
        ));
    }
    Ok(Some(DispatchReceipt {
        receipt_id: raw.0,
        request_id: *id,
        binding_digest: raw
            .1
            .try_into()
            .map_err(|_| Error::Journal("invalid dispatch digest".into()))?,
        engine,
        operation: OperationKind::parse(&raw.3)?,
        conflict: parse_conflict(&raw.4)?,
        state: DispatchState::parse(&raw.5)?,
        job_ids: links.into_iter().map(|(_, job)| job).collect(),
        native_result: raw.6,
    }))
}
fn required_receipt(db: &Connection, id: &[u8; 16]) -> Result<DispatchReceipt> {
    receipt(db, id)?.ok_or_else(|| Error::Journal("dispatch request not accepted".into()))
}

impl Journal {
    /// Look up replay BEFORE rescanning. An existing receipt refers to its frozen
    /// original jobs; a changed filesystem or a new plan never authorizes re-enqueue.
    pub fn dispatch_receipt(&self, id: [u8; 16]) -> Result<Option<DispatchReceipt>> {
        valid_id(&id)?;
        receipt(&self.db, &id)
    }

    /// Frozen envelope for a trusted upstream codec. Core treats it as opaque:
    /// decoding, canonicalization, path/options validation and correspondence to
    /// planner output belong to that codec/planner, not this dedup ledger.
    pub fn dispatch_payload(&self, id: [u8; 16]) -> Result<Option<Vec<u8>>> {
        valid_id(&id)?;
        payload(&self.db, &id)
    }

    /// Persist an explicit choice, never execute it. None cancels before any SQL.
    ///
    /// Trusted boundary: the caller validates/canonicalizes the bounded envelope
    /// and ensures plans represent it. Core binds operation/conflict/engine into
    /// the digest itself; a receipt is neither authorization nor a filesystem
    /// proof. Call dispatch_receipt first on replay, without repeating a scan.
    /// New VartaCopy requests require ready plans; Explorer must have no plans.
    /// Replay returns original jobs without inspecting/replacing supplied plans.
    pub fn accept_dispatch(
        &mut self,
        id: [u8; 16],
        canonical_payload: &[u8],
        operation: OperationKind,
        conflict: Conflict,
        choice: Option<DispatchEngine>,
        plans: &[OperationPlan],
    ) -> Result<Option<DispatchReceipt>> {
        let Some(engine) = choice else {
            return Ok(None);
        };
        valid_id(&id)?;
        if canonical_payload.is_empty() || canonical_payload.len() > MAX_CANONICAL_REQUEST_BYTES {
            return Err(Error::Journal(
                "canonical dispatch payload must contain 1..1048576 bytes".into(),
            ));
        }
        if engine == DispatchEngine::Explorer && !plans.is_empty() {
            return Err(Error::Journal(
                "Explorer dispatch cannot contain VartaCopy plans".into(),
            ));
        }
        let digest = binding(canonical_payload, engine, operation, conflict);
        let tx = self
            .db
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        if let Some(saved) = receipt(&tx, &id)? {
            if saved.binding_digest != digest
                || saved.engine != engine
                || saved.operation != operation
                || conflict_name(saved.conflict) != conflict_name(conflict)
                || payload(&tx, &id)?.as_deref() != Some(canonical_payload)
            {
                return Err(Error::Journal(
                    "dispatch request ID already binds a different payload, engine or options"
                        .into(),
                ));
            }
            tx.commit()?;
            return Ok(Some(saved));
        }
        tx.execute("INSERT INTO dispatch_requests(request_id,canonical_payload,binding_digest,engine,operation,conflict,state) VALUES(?1,?2,?3,?4,?5,?6,'accepted')",
            params![id.as_slice(),canonical_payload,digest.as_slice(),engine.name(),operation.as_str(),conflict_name(conflict)])?;
        let receipt_id = tx.last_insert_rowid();
        if engine == DispatchEngine::VartaCopy {
            let jobs = Self::insert_batch(&tx, plans, conflict, operation)?;
            for (ordinal, job) in jobs.into_iter().enumerate() {
                tx.execute(
                    "INSERT INTO dispatch_jobs(receipt_id,ordinal,job_id) VALUES(?1,?2,?3)",
                    params![receipt_id, sql_num(ordinal)?, job],
                )?;
            }
        }
        let saved = required_receipt(&tx, &id)?;
        tx.commit()?;
        Ok(Some(saved))
    }

    /// Exactly one Accepted Explorer request can acquire this durable intent.
    /// Dispatch may start only after this returns successfully. After a lost
    /// acknowledgement/crash, intent is ambiguous: no automatic retry or fallback.
    pub fn begin_native_dispatch(&mut self, id: [u8; 16]) -> Result<DispatchReceipt> {
        valid_id(&id)?;
        let tx = self
            .db
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let saved = required_receipt(&tx, &id)?;
        if saved.engine != DispatchEngine::Explorer || saved.state != DispatchState::Accepted {
            return Err(Error::Journal("native dispatch is not an unstarted Explorer request; reconcile existing intent/result".into()));
        }
        if tx.execute("UPDATE dispatch_requests SET state='native_intent' WHERE request_id=?1 AND engine='explorer' AND state='accepted'",[id.as_slice()])?!=1 {
            return Err(Error::Journal("native dispatch intent was already acquired".into()));
        }
        let saved = required_receipt(&tx, &id)?;
        tx.commit()?;
        Ok(saved)
    }

    /// Record a bounded trusted-adapter attempt result, not filesystem success.
    /// Equal repeated acknowledgements are idempotent; changed results reject.
    pub fn finish_native_dispatch(
        &mut self,
        id: [u8; 16],
        result: &[u8],
    ) -> Result<DispatchReceipt> {
        valid_id(&id)?;
        if result.is_empty() || result.len() > MAX_NATIVE_RESULT_BYTES {
            return Err(Error::Journal(
                "native result must contain 1..65536 bytes".into(),
            ));
        }
        let tx = self
            .db
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let saved = required_receipt(&tx, &id)?;
        if saved.engine != DispatchEngine::Explorer {
            return Err(Error::Journal(
                "native result requires Explorer engine".into(),
            ));
        }
        if saved.state == DispatchState::NativeFinished
            && saved.native_result.as_deref() == Some(result)
        {
            tx.commit()?;
            return Ok(saved);
        }
        if saved.state != DispatchState::NativeIntent {
            return Err(Error::Journal(
                "native result requires an unfinished dispatch intent".into(),
            ));
        }
        tx.execute("UPDATE dispatch_requests SET state='native_finished',native_result=?2 WHERE request_id=?1 AND state='native_intent'",params![id.as_slice(),result])?;
        let saved = required_receipt(&tx, &id)?;
        tx.commit()?;
        Ok(saved)
    }

    /// Discover accepted or potentially started Explorer requests after restart,
    /// including unknown request IDs. Intent always requires reconciliation.
    /// Pages are bounded; concurrent completions can change later offset pages.
    pub fn unresolved_native_dispatches(
        &self,
        limit: usize,
        offset: usize,
    ) -> Result<DispatchPage> {
        if limit == 0 || limit > 128 {
            return Err(Error::Journal(
                "native dispatch page must contain 1..128 records".into(),
            ));
        }
        let mut query=self.db.prepare("SELECT request_id FROM dispatch_requests WHERE engine='explorer' AND state IN ('accepted','native_intent') ORDER BY receipt_id LIMIT ?1 OFFSET ?2")?;
        let ids = query
            .query_map(params![sql_num(limit + 1)?, sql_num(offset)?], |r| {
                r.get::<_, Vec<u8>>(0)
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        let has_more = ids.len() > limit;
        let receipts = ids
            .into_iter()
            .take(limit)
            .map(|id| {
                let id = id
                    .try_into()
                    .map_err(|_| Error::Journal("invalid stored dispatch ID".into()))?;
                required_receipt(&self.db, &id)
            })
            .collect::<Result<Vec<_>>>()?;
        Ok(DispatchPage {
            receipts,
            offset,
            has_more,
        })
    }
}
