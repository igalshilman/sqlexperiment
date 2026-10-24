//! The scope's database, as a turn sees it: one SQL transaction, begun at the
//! turn's first statement and committed with the turn (its records and pages,
//! one flush). `ctx.kv` is a table in it, and `ctx.db.sql` runs in it.
//!
//! The guest reaches it synchronously, through the VM's host call (see
//! [`crate::vm`]), with one JSON request and one JSON answer. A statement
//! never ends a turn: if it reads pages from Restate, the guest waits on its
//! fiber. An SQL error (syntax, a constraint) undoes the statement and throws
//! in the guest; a storage error fails the attempt, which runs again from the
//! last seal.
//!
//! `ctx.db.transaction(fn)` is a savepoint, released when `fn` returns and
//! rolled back when it throws. `fn` is synchronous, so a transaction never
//! spans a seal. Statements that end or nest transactions themselves
//! (`BEGIN`, `COMMIT`, `SAVEPOINT`, ...) are refused: they would commit the
//! SQL without the turn.
use crate::{
    model::MAX_INPUT,
    store::storage_error,
    transaction::{Participant, State, Wal},
};
use anyhow::{Context, Result, bail, ensure};
use serde_json::{Map, Number, Value, json};
use sql::{Sql, Value as SqlValue};

/// The scope's KV, a table of its database: values are JSON.
const KV_TABLE: &str = "CREATE TABLE IF NOT EXISTS kv(key TEXT PRIMARY KEY, value TEXT NOT NULL)";
/// Statements that would end or nest the turn's transaction.
const TRANSACTION_CONTROL: [&str; 6] =
    ["begin", "commit", "end", "rollback", "savepoint", "release"];

/// The scope's database, and where this turn's transaction stands.
pub struct Db {
    sql: Sql,
    /// Whether the turn's transaction is open.
    began: bool,
    /// Whether this session made sure the KV table exists.
    kv_table: bool,
    /// Savepoints `ctx.db.transaction` holds open, innermost last.
    savepoints: u32,
}

impl Db {
    pub fn new(sql: Sql) -> Self {
        Self {
            sql,
            began: false,
            kv_table: false,
            savepoints: 0,
        }
    }

    /// The scope's state: the records and files sealed with its SQL.
    pub fn state(&self) -> State {
        self.sql.state()
    }

    /// The guest trapped: what its open `ctx.db.transaction` did is undone,
    /// as if it had thrown. What the turn did before stands.
    pub async fn abandon_transactions(&mut self) -> Result<()> {
        if self.savepoints > 0 {
            self.internal("ROLLBACK TO qjs_1").await?;
            self.internal("RELEASE qjs_1").await?;
            self.savepoints = 0;
        }
        Ok(())
    }

    async fn begin(&mut self) -> Result<()> {
        if !self.began {
            self.internal("BEGIN").await?;
            self.began = true;
        }
        Ok(())
    }

    async fn internal(&mut self, statement: &str) -> Result<()> {
        self.sql.execute(statement).await.map_err(storage_error)
    }

    async fn kv_table(&mut self) -> Result<()> {
        self.begin().await?;
        if !self.kv_table {
            self.internal(KV_TABLE).await?;
            self.kv_table = true;
        }
        Ok(())
    }

    /// The value of `key`, as this turn sees it.
    pub async fn kv_read(&mut self, key: &str) -> Result<Value> {
        self.kv_table().await?;
        let select = "SELECT value FROM kv WHERE key = ?";
        let key = SqlValue::build_text(key.to_owned());
        let rows = self
            .sql
            .query_with(select, [key])
            .await
            .map_err(storage_error)?;
        let text = match rows.first().and_then(|row| row.first()) {
            None => return Ok(Value::Null),
            Some(SqlValue::Text(text)) => text.as_str(),
            Some(_) => bail!("KV value is not text"),
        };
        ensure!(text.len() <= MAX_INPUT, "KV value exceeds limit");
        Ok(serde_json::from_str(text)?)
    }

    /// Set `key` to `value`, or with `None` delete it, in this turn.
    pub async fn kv_write(&mut self, key: &str, value: Option<&Value>) -> Result<Value> {
        self.kv_table().await?;
        let key = SqlValue::build_text(key.to_owned());
        match value {
            Some(value) => {
                let value = SqlValue::build_text(serde_json::to_string(value)?);
                let set = "INSERT INTO kv VALUES(?, ?) ON CONFLICT(key) DO UPDATE SET value = excluded.value";
                self.sql.execute_with(set, [key, value]).await
            }
            None => (self.sql)
                .execute_with("DELETE FROM kv WHERE key = ?", [key])
                .await,
        }
        .map_err(storage_error)?;
        Ok(Value::Null)
    }

    /// The rows `statement` selects with `params`, in the turn's transaction,
    /// as the host asks it: any statement but the transaction control ones.
    pub async fn query(&mut self, statement: &str, params: &[Value]) -> Result<Vec<Vec<Value>>> {
        let params = (params.iter().map(to_sql).collect::<Result<Vec<_>, _>>())
            .map_err(|message| anyhow::anyhow!(message))?;
        self.begin().await?;
        let rows = self
            .sql
            .query_with(statement, params)
            .await
            .map_err(storage_error)?;
        Ok((rows.into_iter())
            .map(|row| row.into_iter().map(from_sql).collect())
            .collect())
    }

    /// Answer a guest's host call: `{ok: true, value}`, or `{ok: false,
    /// error}` for an error the guest throws. An `Err` is the host's: it fails
    /// the attempt.
    pub async fn call(&mut self, operation: &str, request: Value) -> Result<Value> {
        let answer = match operation {
            "db.sql" => {
                let statement = request["sql"].as_str().context("Missing SQL")?;
                let params = request["params"].as_array().context("Missing parameters")?;
                self.statement(statement, params).await?
            }
            "db.begin" => {
                self.begin().await?;
                self.savepoints += 1;
                let statement = format!("SAVEPOINT qjs_{}", self.savepoints);
                self.internal(&statement).await?;
                Ok(Value::Null)
            }
            "db.commit" | "db.rollback" => {
                ensure!(self.savepoints > 0, "No transaction is open");
                let name = format!("qjs_{}", self.savepoints);
                if operation == "db.rollback" {
                    self.internal(&format!("ROLLBACK TO {name}")).await?;
                }
                self.internal(&format!("RELEASE {name}")).await?;
                self.savepoints -= 1;
                Ok(Value::Null)
            }
            _ => bail!("No host call {operation}"),
        };
        Ok(match answer {
            Ok(value) => json!({"ok": true, "value": value}),
            Err(message) => json!({"ok": false, "error": {"name": "SQLError", "message": message}}),
        })
    }

    /// One statement, its rows as objects; `Err` holds an SQL error's message.
    async fn statement(
        &mut self,
        statement: &str,
        params: &[Value],
    ) -> Result<Result<Value, String>> {
        if let Some(keyword) = first_keyword(statement)
            && TRANSACTION_CONTROL.contains(&keyword.as_str())
        {
            return Ok(Err(format!(
                "{} is not allowed: use ctx.db.transaction(fn)",
                keyword.to_uppercase()
            )));
        }
        let params = match params.iter().map(to_sql).collect::<Result<Vec<_>, _>>() {
            Ok(params) => params,
            Err(message) => return Ok(Err(message)),
        };
        self.begin().await?;
        // With parameters given, even none, the SQL must be one statement.
        match self.sql.query_columns(statement, Some(params)).await {
            Ok((columns, rows)) => Ok(Ok(Value::Array(
                rows.into_iter()
                    .map(|row| {
                        let row = columns.iter().cloned().zip(row.into_iter().map(from_sql));
                        Value::Object(row.collect::<Map<_, _>>())
                    })
                    .collect(),
            ))),
            Err(error) if self.sql.is_usable() => Ok(Err(error.to_string())),
            Err(error) => Err(storage_error(error)),
        }
    }
}

/// The statement's first keyword, lowercased, past whitespace and comments.
fn first_keyword(statement: &str) -> Option<String> {
    let mut rest = statement;
    loop {
        rest = rest.trim_start();
        if let Some(line) = rest.strip_prefix("--") {
            rest = line.split_once('\n').map_or("", |(_, after)| after);
        } else if let Some(block) = rest.strip_prefix("/*") {
            rest = block.split_once("*/").map_or("", |(_, after)| after);
        } else {
            break;
        }
    }
    let word: String = rest.chars().take_while(char::is_ascii_alphabetic).collect();
    (!word.is_empty()).then(|| word.to_ascii_lowercase())
}

/// A parameter, as the guest sends it: JSON, with `{"$int": "..."}` for a
/// bigint and `{"$blob": "<hex>"}` for bytes.
fn to_sql(value: &Value) -> Result<SqlValue, String> {
    Ok(match value {
        Value::Null => SqlValue::Null,
        Value::Bool(b) => SqlValue::from_i64(i64::from(*b)),
        Value::Number(n) => match n.as_i64() {
            Some(i) => SqlValue::from_i64(i),
            None => {
                let f = n.as_f64().ok_or("Invalid number")?;
                if f.fract() == 0.0 && f.abs() < 9.2e18 {
                    SqlValue::from_i64(f as i64)
                } else {
                    SqlValue::from_f64(f)
                }
            }
        },
        Value::String(text) => SqlValue::build_text(text.clone()),
        Value::Object(tagged) => match (tagged.get("$int"), tagged.get("$blob")) {
            (Some(Value::String(int)), None) => SqlValue::from_i64(
                int.parse()
                    .map_err(|_| "a bigint parameter exceeds 64 bits")?,
            ),
            (None, Some(Value::String(hex))) => {
                SqlValue::from_slice(&unhex(hex)?).map_err(|e| e.to_string())?
            }
            _ => return Err("unsupported SQL parameter: an object".into()),
        },
        Value::Array(_) => return Err("unsupported SQL parameter: an array".into()),
    })
}

/// A result value, tagged as [`to_sql`] reads it.
fn from_sql(value: SqlValue) -> Value {
    const SAFE: i64 = (1 << 53) - 1;
    match value {
        SqlValue::Null => Value::Null,
        SqlValue::Numeric(sql::Numeric::Integer(i)) if (-SAFE..=SAFE).contains(&i) => {
            Value::from(i)
        }
        SqlValue::Numeric(sql::Numeric::Integer(i)) => json!({"$int": i.to_string()}),
        SqlValue::Numeric(sql::Numeric::Float(f)) => {
            Number::from_f64(f64::from(f)).map_or(Value::Null, Value::Number)
        }
        SqlValue::Text(text) => Value::String(text.as_str().to_owned()),
        SqlValue::Blob(blob) => json!({"$blob": hex(&blob)}),
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn unhex(hex: &str) -> Result<Vec<u8>, String> {
    if !hex.len().is_multiple_of(2) {
        return Err("invalid blob".into());
    }
    (0..hex.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).map_err(|_| "invalid blob".into()))
        .collect()
}

/// The scope's tables, a participant: their part of the seal is the turn's
/// SQL transaction, which the seal's flush makes durable.
#[async_trait::async_trait]
impl Participant for Db {
    /// The turn's transaction, committed and moved into `wal`, as pages of
    /// the state's file `db`; the next statement begins the next turn's.
    async fn prepare(&mut self, wal: &mut Wal) -> Result<()> {
        ensure!(self.savepoints == 0, "A transaction is still open");
        self.began = false;
        self.sql.flush(wal).await.map_err(storage_error)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transaction_control_is_found_past_comments() {
        for statement in [
            "COMMIT",
            "  commit;",
            "-- a\n end",
            "/* x */ Rollback",
            "savepoint s",
        ] {
            assert!(TRANSACTION_CONTROL.contains(&first_keyword(statement).unwrap().as_str()));
        }
        assert_eq!(first_keyword("SELECT 1").as_deref(), Some("select"));
        assert_eq!(first_keyword("-- only a comment"), None);
    }

    #[test]
    fn values_round_trip_through_their_tags() {
        for value in [
            json!(null),
            json!(7),
            json!(1.5),
            json!("text"),
            json!({"$int": "9007199254740993"}),
            json!({"$blob": "00ff10"}),
        ] {
            assert_eq!(from_sql(to_sql(&value).unwrap()), value);
        }
        assert_eq!(from_sql(to_sql(&json!(true)).unwrap()), json!(1));
        assert!(to_sql(&json!([1])).is_err());
    }
}
