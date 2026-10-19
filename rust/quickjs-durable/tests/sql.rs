//! `ctx.db`: SQL a program runs synchronously, in the turn's transaction,
//! over the scope's database.
use anyhow::Result;
use common::*;
use quickjs_durable::{Outcome, Status};
use serde_json::{Value, json};

mod common;

/// The run's outcome: its value, or its error's name and message.
fn outcome(run: &mut quickjs_durable::Run) -> Result<Value> {
    let view = finish(run)?;
    Ok(match view.state.outcome {
        Some(Outcome::Success { value, .. }) => value,
        Some(Outcome::Failure { error, .. }) => {
            json!({"error": error.name, "message": error.message})
        }
        None => unreachable!(),
    })
}

#[test]
fn statements_return_rows_and_survive_a_restore() -> Result<()> {
    let source = r#"export async function main(_,ctx){
      ctx.db.sql`CREATE TABLE notes(id INTEGER PRIMARY KEY, text TEXT, score REAL)`;
      for (const text of ['milk','eggs']) ctx.db.sql`INSERT INTO notes(text, score) VALUES(${text}, ${text.length/2})`;
      await ctx.sleep(1);
      // Another attempt restores the heap; the table is the database's.
      ctx.db.query('UPDATE notes SET text = upper(text) WHERE id = ?', [2]);
      return ctx.db.sql`SELECT id, text, score FROM notes ORDER BY id`;
    }"#;
    let store = Store::default();
    start(&store, source, Value::Null, 1)?;
    let mut run = start(&store, source, Value::Null, 2)?;
    assert_eq!(
        outcome(&mut run)?,
        json!([{"id":1,"text":"milk","score":2},{"id":2,"text":"EGGS","score":2}])
    );
    Ok(())
}

#[test]
fn values_keep_their_types() -> Result<()> {
    let source = r#"export async function main(_,ctx){
      const [row]=ctx.db.sql`SELECT ${null} AS none, ${true} AS yes, ${9007199254740993n} AS big,
        ${new Uint8Array([0,255])} AS bytes, ${1.5} AS real, ${'é'} AS text`;
      return [row.none, row.yes, typeof row.big, String(row.big), Array.from(row.bytes), row.real, row.text];
    }"#;
    let store = Store::default();
    let mut run = start(&store, source, Value::Null, 1)?;
    assert_eq!(
        outcome(&mut run)?,
        json!([null, 1, "bigint", "9007199254740993", [0, 255], 1.5, "é"])
    );
    Ok(())
}

/// An SQL error throws, and undoes only its statement.
#[test]
fn an_sql_error_throws_in_the_program() -> Result<()> {
    let source = r#"export async function main(_,ctx){
      ctx.db.sql`CREATE TABLE t(id INTEGER PRIMARY KEY)`;
      ctx.db.sql`INSERT INTO t VALUES(1)`;
      let caught;
      try { ctx.db.sql`INSERT INTO t VALUES(1)`; } catch (error) { caught = error.name; }
      ctx.db.sql`INSERT INTO t VALUES(2)`;
      return [caught, ctx.db.sql`SELECT count(*) AS n FROM t`[0].n];
    }"#;
    let store = Store::default();
    let mut run = start(&store, source, Value::Null, 1)?;
    assert_eq!(outcome(&mut run)?, json!(["SQLError", 2]));
    Ok(())
}

#[test]
fn a_transaction_stands_or_falls_whole() -> Result<()> {
    let source = r#"export async function main(_,ctx){
      ctx.db.sql`CREATE TABLE t(n INTEGER)`;
      const value=ctx.db.transaction(()=>{ctx.db.sql`INSERT INTO t VALUES(1)`;return 'kept';});
      let caught;
      try {
        ctx.db.transaction(()=>{
          ctx.db.sql`INSERT INTO t VALUES(2)`;
          // A nested one, undone with the outer.
          ctx.db.transaction(()=>ctx.db.sql`INSERT INTO t VALUES(3)`);
          throw new Error('undo');
        });
      } catch (error) { caught = error.message; }
      let awaited;
      try { ctx.db.transaction(async ()=>{}); } catch (error) { awaited = error.name; }
      return [value, caught, awaited, ctx.db.sql`SELECT n FROM t`.map(r=>r.n)];
    }"#;
    let store = Store::default();
    let mut run = start(&store, source, Value::Null, 1)?;
    assert_eq!(
        outcome(&mut run)?,
        json!(["kept", "undo", "TypeError", [1]])
    );
    Ok(())
}

/// A statement that would commit the SQL without the turn is refused.
#[test]
fn transaction_statements_are_refused() -> Result<()> {
    let source = r#"export async function main(_,ctx){
      const refused=[];
      for (const sql of ['BEGIN','COMMIT','/* x */ rollback','SAVEPOINT s','RELEASE s','END'])
        try { ctx.db.query(sql); } catch (error) { refused.push(error.name); }
      return refused;
    }"#;
    let store = Store::default();
    let mut run = start(&store, source, Value::Null, 1)?;
    assert_eq!(outcome(&mut run)?, json!(vec!["SQLError"; 6]));
    Ok(())
}

/// `ctx.kv` is a table of the same database, in the same transaction.
#[test]
fn kv_and_sql_see_each_other() -> Result<()> {
    let source = r#"export async function main(_,ctx){
      await ctx.kv.set('a', {n: 1});
      const [row]=ctx.db.sql`SELECT value FROM kv WHERE key = 'a'`;
      ctx.db.sql`UPDATE kv SET value = '{"n":2}' WHERE key = 'a'`;
      return [JSON.parse(row.value).n, (await ctx.kv.get('a')).n];
    }"#;
    let store = Store::default();
    let mut run = start(&store, source, Value::Null, 1)?;
    assert_eq!(outcome(&mut run)?, json!([1, 2]));
    Ok(())
}

/// A trap inside a transaction undoes it, as a throw would; what the turn
/// did before stands.
#[test]
fn a_trap_in_a_transaction_undoes_it() -> Result<()> {
    let source = r#"export async function main(_,ctx){
      ctx.db.sql`CREATE TABLE t(n INTEGER)`;
      ctx.db.sql`INSERT INTO t VALUES(1)`;
      ctx.db.transaction(()=>{ctx.db.sql`INSERT INTO t VALUES(2)`;while(true){}});
    }"#;
    let store = Store::default();
    let run = start(&store, source, Value::Null, 1)?;
    assert_eq!(run.view().state.status, Status::Failed);
    // Another run in the scope reads what stood.
    let probe =
        "export async function main(_,ctx){return ctx.db.sql`SELECT n FROM t`.map(r=>r.n);}";
    let mut run = block(quickjs_durable::Run::start(
        &programs(probe),
        &store.scope(),
        "s",
        "probe",
        "v1",
        "main",
        Value::Null,
        2,
    ))?;
    assert_eq!(outcome(&mut run)?, json!([1]));
    Ok(())
}

/// A turn is bounded by time, not instructions: thousands of statements in
/// one synchronous job are fine.
#[test]
fn a_job_runs_thousands_of_statements() -> Result<()> {
    let source = r#"export async function main(_,ctx){
      ctx.db.sql`CREATE TABLE t(n INTEGER)`;
      ctx.db.transaction(()=>{for(let i=0;i<3000;i++) ctx.db.sql`INSERT INTO t VALUES(${i})`;});
      return ctx.db.sql`SELECT count(*) AS n, sum(n) AS total FROM t`[0];
    }"#;
    let store = Store::default();
    let mut run = start(&store, source, Value::Null, 1)?;
    assert_eq!(outcome(&mut run)?, json!({"n": 3000, "total": 4498500}));
    Ok(())
}
