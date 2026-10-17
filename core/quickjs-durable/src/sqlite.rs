//! Small single-writer SQLite host. One SQLite transaction owns checkpoint KV,
//! scoped application state, effect registration and completion consumption.
//!
//! WAL + synchronous=FULL makes each successful COMMIT the durability boundary.
//! SQLite's WAL checkpoint is separate maintenance; it is not a VM checkpoint.
//! The host drives everything on its own thread. SQLite calls are synchronous;
//! the engine's awaitable guest callback also allows a different host to perform
//! remote transactional I/O without snapshotting a Rust future.
use crate::{
    Kv, Operation, Outcome, Resolution, Transaction,
    model::{MAX_INPUT, validate_time},
};
use anyhow::{Context, Result, ensure};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use serde::Serialize;
use serde_json::Value;
use std::{cell::Cell, path::Path, time::Instant};

/// The only writer connection. No worker thread, pool, scheduler or replay log.
/// The embedding drives dispatch and chooses which ready effect to deliver.
pub struct Host {
    connection: Connection,
}

/// Current recoverable effect, not a historical operation receipt. `outcome`
/// remains available until consumption commits together with the resumed VM.
#[derive(Clone, Debug, Serialize)]
pub struct Effect {
    pub operation: Operation,
    pub outcome: Option<Outcome>,
    pub completed_at: Option<u64>,
}

/// Logical checkpoint I/O and application requests for one transaction. These
/// byte counts exclude SQLite pages/WAL framing and are not physical disk I/O.
#[derive(Clone, Copy, Debug, Default, Serialize)]
pub struct IoStats {
    pub checkpoint_reads: usize,
    pub checkpoint_read_bytes: usize,
    pub checkpoint_writes: usize,
    pub checkpoint_write_bytes: usize,
    pub kv_reads: usize,
    pub kv_writes: usize,
    pub effect_inserts: usize,
    pub effect_consumptions: usize,
}

#[derive(Clone, Debug, Serialize)]
pub struct CommitStats {
    pub commit_ms: f64,
    pub io: IoStats,
}

/// A borrowed SQLite transaction. Dropping it rolls back all staged work.
/// After an engine error or cancelled engine future, drop the engine/transaction;
/// do not extract and commit a partially executed turn.
pub struct SqliteTransaction<'a> {
    sql: rusqlite::Transaction<'a>,
    run: String,
    scope: String,
    io: Cell<IoStats>,
}

impl Host {
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let connection = Connection::open(path)?;
        connection.busy_timeout(std::time::Duration::ZERO)?;
        let mode: String = connection.query_row("PRAGMA journal_mode=WAL", [], |r| r.get(0))?;
        ensure!(
            mode == "wal",
            "SQLite host requires a file-backed WAL database"
        );
        connection.execute_batch(
            "PRAGMA synchronous=FULL;
             PRAGMA wal_autocheckpoint=1000;
             CREATE TABLE IF NOT EXISTS checkpoints (
               key TEXT PRIMARY KEY, value BLOB NOT NULL
             ) WITHOUT ROWID;
             CREATE TABLE IF NOT EXISTS state (
               scope TEXT NOT NULL, key TEXT NOT NULL, value BLOB NOT NULL,
               PRIMARY KEY(scope,key)
             ) WITHOUT ROWID;
             CREATE TABLE IF NOT EXISTS runs (
               run TEXT PRIMARY KEY, scope TEXT NOT NULL
             ) WITHOUT ROWID;
             CREATE TABLE IF NOT EXISTS effects (
               id TEXT PRIMARY KEY, run TEXT NOT NULL, operation BLOB NOT NULL,
               outcome BLOB, completed_at INTEGER,
               CHECK((outcome IS NULL) = (completed_at IS NULL))
             );
             CREATE INDEX IF NOT EXISTS effects_run ON effects(run);",
        )?;
        ensure!(
            connection.query_row("PRAGMA synchronous", [], |r| r.get::<_, i64>(0))? == 2,
            "SQLite synchronous=FULL was not applied"
        );
        Ok(Self { connection })
    }

    /// Reserve the sole writer before running JavaScript. Remote transactional
    /// reads may yield while this guard remains live; no other writer can commit.
    pub fn begin(&mut self, run: &str, scope: &str) -> Result<SqliteTransaction<'_>> {
        let sql = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        sql.execute(
            "INSERT INTO runs(run,scope) VALUES(?,?) ON CONFLICT(run) DO NOTHING",
            params![run, scope],
        )?;
        let bound: String =
            sql.query_row("SELECT scope FROM runs WHERE run=?", [run], |r| r.get(0))?;
        ensure!(
            bound == scope,
            "Run belongs to a different application scope"
        );
        Ok(SqliteTransaction {
            sql,
            run: run.into(),
            scope: scope.into(),
            io: Cell::new(IoStats::default()),
        })
    }

    /// Read only committed effect state, outside a VM transaction. Dispatch only
    /// rows without an outcome, using operation.id() as the stable effect ID.
    pub fn effects(&self, run: &str) -> Result<Vec<Effect>> {
        let mut query = self.connection.prepare_cached(
            "SELECT operation,outcome,completed_at FROM effects WHERE run=? ORDER BY id",
        )?;
        let mut rows = query.query([run])?;
        let mut result = Vec::new();
        while let Some(row) = rows.next()? {
            let operation: Vec<u8> = row.get(0)?;
            let outcome: Option<Vec<u8>> = row.get(1)?;
            result.push(Effect {
                operation: serde_json::from_slice(&operation)?,
                outcome: outcome.map(|b| serde_json::from_slice(&b)).transpose()?,
                completed_at: row
                    .get::<_, Option<i64>>(2)?
                    .map(u64::try_from)
                    .transpose()?,
            });
        }
        Ok(result)
    }

    /// Make an externally obtained result available for delivery. This separate
    /// short transaction does not advance the VM. First result wins; a duplicate
    /// after consumption is ignored. This does not make arbitrary remote effects
    /// exactly once: the transport/receiver must honor the stable operation ID.
    pub fn record_result(&mut self, id: &str, outcome: &Outcome, now: u64) -> Result<bool> {
        validate_time(now)?;
        outcome.validate()?;
        let bytes = serde_json::to_vec(outcome)?;
        ensure!(bytes.len() <= MAX_INPUT, "Host result exceeds limit");
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let operation: Option<Vec<u8>> = tx
            .query_row(
                "SELECT operation FROM effects WHERE id=? AND outcome IS NULL",
                [id],
                |r| r.get(0),
            )
            .optional()?;
        let Some(operation) = operation else {
            return Ok(false);
        };
        if let Operation::Sleep { deadline, .. } = serde_json::from_slice(&operation)? {
            ensure!(now >= deadline, "Timer completed before deadline");
        }
        let changed = tx.execute(
            "UPDATE effects SET outcome=?,completed_at=? WHERE id=? AND outcome IS NULL",
            params![bytes, i64::try_from(now)?, id],
        )?;
        tx.commit()?;
        Ok(changed == 1)
    }

    pub fn state(&self, scope: &str, key: &str) -> Result<Option<Value>> {
        state(&self.connection, scope, key)
    }
}

fn state(connection: &Connection, scope: &str, key: &str) -> Result<Option<Value>> {
    let bytes: Option<Vec<u8>> = connection
        .prepare_cached("SELECT value FROM state WHERE scope=? AND key=?")?
        .query_row(params![scope, key], |r| r.get(0))
        .optional()?;
    Ok(bytes.map(|b| serde_json::from_slice(&b)).transpose()?)
}

impl SqliteTransaction<'_> {
    pub fn commit(self) -> Result<CommitStats> {
        let io = self.io.get();
        let before = Instant::now();
        self.sql.commit()?;
        Ok(CommitStats {
            commit_ms: before.elapsed().as_secs_f64() * 1000.0,
            io,
        })
    }

    pub fn io(&self) -> IoStats {
        self.io.get()
    }

    fn count(&self, update: impl FnOnce(&mut IoStats)) {
        let mut stats = self.io.get();
        update(&mut stats);
        self.io.set(stats);
    }
}

impl Kv for SqliteTransaction<'_> {
    async fn get(&mut self, key: &str) -> Result<Option<Vec<u8>>> {
        let value: Option<Vec<u8>> = self
            .sql
            .prepare_cached("SELECT value FROM checkpoints WHERE key=?")?
            .query_row([key], |r| r.get(0))
            .optional()?;
        self.count(|s| {
            s.checkpoint_reads += 1;
            s.checkpoint_read_bytes += value.as_ref().map_or(0, Vec::len);
        });
        Ok(value)
    }

    async fn put(&mut self, key: &str, value: &[u8]) -> Result<()> {
        self.sql.prepare_cached(
            "INSERT INTO checkpoints(key,value) VALUES(?,?) ON CONFLICT(key) DO UPDATE SET value=excluded.value",
        )?.execute(params![key, value])?;
        self.count(|s| {
            s.checkpoint_writes += 1;
            s.checkpoint_write_bytes += key.len() + value.len();
        });
        Ok(())
    }
}

impl Transaction for SqliteTransaction<'_> {
    fn check_run(&self, run: &str) -> Result<()> {
        ensure!(run == self.run, "Transaction belongs to a different run");
        Ok(())
    }
    async fn execute(&mut self, operation: &Operation) -> Result<Resolution> {
        ensure!(
            operation.id().rsplit_once('/').map(|p| p.0) == Some(self.run.as_str()),
            "Transaction belongs to a different run"
        );
        let value = match operation {
            Operation::KvGet { key, .. } => {
                self.count(|s| s.kv_reads += 1);
                state(&self.sql, &self.scope, key)?.unwrap_or(Value::Null)
            }
            Operation::KvSet { key, value, .. } => {
                self.sql.prepare_cached(
                    "INSERT INTO state(scope,key,value) VALUES(?,?,?) ON CONFLICT(scope,key) DO UPDATE SET value=excluded.value",
                )?.execute(params![self.scope, key, serde_json::to_vec(value)?])?;
                self.count(|s| s.kv_writes += 1);
                Value::Null
            }
            Operation::KvDelete { key, .. } => {
                self.sql
                    .prepare_cached("DELETE FROM state WHERE scope=? AND key=?")?
                    .execute(params![self.scope, key])?;
                self.count(|s| s.kv_writes += 1);
                Value::Null
            }
            _ => {
                // A host-scheduled resume wakeup is ready as soon as
                // this whole transaction commits. There is no dispatch round trip.
                let ready = matches!(operation, Operation::Resume { .. });
                let outcome = ready
                    .then(|| serde_json::to_vec(&Outcome::success(Value::Null)))
                    .transpose()?;
                self.sql.prepare_cached(
                    "INSERT INTO effects(id,run,operation,outcome,completed_at) VALUES(?,?,?,?,?)",
                )?.execute(params![operation.id(), self.run, serde_json::to_vec(operation)?, outcome, ready.then_some(0)])?;
                self.count(|s| s.effect_inserts += 1);
                return Ok(Resolution::Pending);
            }
        };
        Ok(Resolution::Ready(Outcome::success(value)))
    }

    async fn acknowledge(&mut self, id: &str, outcome: &Outcome, now: u64) -> Result<()> {
        let recorded: Option<(Vec<u8>, i64)> = self.sql.query_row(
            "SELECT outcome,completed_at FROM effects WHERE id=? AND run=? AND outcome IS NOT NULL",
            params![id, self.run], |r| Ok((r.get(0)?, r.get(1)?))
        ).optional()?;
        let (bytes, completed_at) =
            recorded.context("Completion is not available in this transaction")?;
        ensure!(
            serde_json::from_slice::<Outcome>(&bytes)? == *outcome,
            "Delivery does not match recorded outcome"
        );
        ensure!(
            now >= u64::try_from(completed_at)?,
            "Delivery precedes recorded result time"
        );
        let deleted = self
            .sql
            .prepare_cached("DELETE FROM effects WHERE id=? AND run=? AND outcome IS NOT NULL")?
            .execute(params![id, self.run])?;
        ensure!(
            deleted == 1,
            "Completion is not available in this transaction"
        );
        self.count(|s| s.effect_consumptions += 1);
        Ok(())
    }
}
