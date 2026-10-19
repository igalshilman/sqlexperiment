//! The scope's database, as a turn sees it: one SQL transaction, begun at the
//! turn's first statement and committed with the turn (its state and page
//! batch, one put). `ctx.kv` is a table in it, and `ctx.db.sql` runs in it.
//!
//! The guest reaches it synchronously, through the VM's host call (see
//! [`crate::vm`]), with one JSON request and one JSON answer. A statement
//! never ends a turn: if it reads pages from Restate, the guest waits on its
//! fiber. An SQL error (syntax, a constraint) undoes the statement and throws
//! in the guest; a storage error fails the attempt, which runs again from the
//! last commit.
//!
//! `ctx.db.transaction(fn)` is a savepoint, released when `fn` returns and
//! rolled back when it throws. `fn` is synchronous, so a transaction never
//! spans a commit. Statements that end or nest transactions themselves
//! (`BEGIN`, `COMMIT`, `SAVEPOINT`, ...) are refused: they would commit the
//! SQL without the turn.
use crate::model::MAX_INPUT;
use anyhow::{Context, Result, bail, ensure};
use serde_json::{Map, Number, Value, json};
use sqlite_kv::{Session, Value as SqlValue};

/// The scope's KV, a table of its database: values are JSON.
const KV_TABLE: &str = "CREATE TABLE IF NOT EXISTS kv(key TEXT PRIMARY KEY, value TEXT NOT NULL)";
/// Statements that would end or nest the turn's transaction.
const TRANSACTION_CONTROL: [&str; 6] =
    ["begin", "commit", "end", "rollback", "savepoint", "release"];

fn sql(error: Box<dyn std::error::Error + Send + Sync>) -> anyhow::Error {
    anyhow::anyhow!(error)
}

/// The scope's database, and where this turn's transaction stands.
pub struct Db {
    session: Session,
    /// Whether the turn's transaction is open.
    began: bool,
    /// Whether this session made sure the KV table exists.
    kv_table: bool,
    /// Savepoints `ctx.db.transaction` holds open, innermost last.
    savepoints: u32,
}

impl Db {
    pub fn new(session: Session) -> Self {
        Self {
            session,
            began: false,
            kv_table: false,
            savepoints: 0,
        }
    }

    pub fn session(&self) -> &Session {
        &self.session
    }

    /// Commit the turn: its SQL, if any, with `effects` and the chain update,
    /// in one put. The next statement begins the next turn's transaction.
    pub async fn commit_turn(
        &mut self,
        effects: bytes::Bytes,
        chain: Option<sqlite_kv::ChainUpdate>,
    ) -> Result<()> {
        ensure!(self.savepoints == 0, "A transaction is still open");
        self.began = false;
        match chain {
            Some(chain) => self.session.commit_turn(effects, chain).await,
            None => self.session.commit_with(effects).await,
        }
        .map_err(sql)
    }

    /// The guest trapped, and its memory goes back to the last commit: so
    /// does its SQL. The turn's transaction is undone.
    pub async fn rollback_turn(&mut self) -> Result<()> {
        if self.began {
            self.internal("ROLLBACK").await?;
        }
        self.began = false;
        self.savepoints = 0;
        // The turn may have created it.
        self.kv_table = false;
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
        self.session.execute(statement).await.map_err(sql)
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
        let rows = self.session.query_with(select, [key]).await.map_err(sql)?;
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
                self.session.execute_with(set, [key, value]).await
            }
            None => (self.session)
                .execute_with("DELETE FROM kv WHERE key = ?", [key])
                .await,
        }
        .map_err(sql)?;
        Ok(Value::Null)
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
        match self.session.query_columns(statement, Some(params)).await {
            Ok((columns, rows)) => Ok(Ok(Value::Array(
                rows.into_iter()
                    .map(|row| {
                        let row = columns.iter().cloned().zip(row.into_iter().map(from_sql));
                        Value::Object(row.collect::<Map<_, _>>())
                    })
                    .collect(),
            ))),
            Err(error) if self.session.is_usable() => Ok(Err(error.to_string())),
            Err(error) => Err(sql(error)),
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
        SqlValue::Numeric(sqlite_kv::Numeric::Integer(i)) if (-SAFE..=SAFE).contains(&i) => {
            Value::from(i)
        }
        SqlValue::Numeric(sqlite_kv::Numeric::Integer(i)) => json!({"$int": i.to_string()}),
        SqlValue::Numeric(sqlite_kv::Numeric::Float(f)) => {
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
