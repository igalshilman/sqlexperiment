//! A cold indexed lookup over an in-memory KV store: how little of a stored
//! database one query fetches. Run with
//! `cargo run --release --features simulation --example lookup`.
use sql::Sql;
use state::{Result, State, blobs::KV_PREFIX, simulation::MemoryKv};
use std::sync::Arc;

/// Flush `db`'s SQL in a mutation of its state, and the mutation, ending the session.
async fn save(mut db: Sql) -> state::Result<state::FlushReport> {
    let state = db.state();
    let mut mutation = state.mutation();
    db.flush(&mut mutation).await?;
    mutation.flush().await
}

fn main() -> Result<()> {
    futures::executor::block_on(async {
        let store = Arc::new(MemoryKv::default());
        let mut db = Sql::open(&State::open(store.clone()).await?).await?;
        db.execute("CREATE TABLE accounts (id INTEGER PRIMARY KEY, email TEXT NOT NULL, payload TEXT NOT NULL)").await?;
        db.execute("CREATE UNIQUE INDEX accounts_email ON accounts(email)")
            .await?;
        db.execute("WITH RECURSIVE n(x) AS (VALUES(1) UNION ALL SELECT x+1 FROM n WHERE x < 12000) INSERT INTO accounts SELECT x, printf('user%05d@example.com', x), hex(randomblob(250)) FROM n").await?;
        let local = db.io_stats();
        assert_eq!(store.stats().puts, 0);
        // A large save: one tail entry holding every page, then the head.
        save(db).await?;
        let saved = store.stats();
        // The next flush checkpoints it: frames of pages, in packs.
        let state = State::open(store.clone()).await?;
        let mut mutation = state.mutation();
        mutation.set(&state.record("loaded"), "yes".into())?;
        let checkpoint = mutation.flush().await?.checkpoint.expect("a checkpoint");
        store.crash();
        println!(
            "Private WAL bytes written locally: {}",
            local.private_wal_bytes_written
        );
        println!("Backend puts before save: 0");
        println!("Backend bytes written by save: {}", saved.bytes_written);
        println!("Backend puts by save: {}", saved.puts);

        let parts = store
            .keys()
            .iter()
            .filter(|k| k.starts_with(KV_PREFIX))
            .count();
        let stored_bytes = store.stored_bytes();
        store.reset_stats();
        // New engine, pager, file handles and I/O adapter; only KV bytes survive.
        let mut cold = Sql::open(&State::open(store.clone()).await?).await?;
        let query =
            "SELECT id, length(payload) FROM accounts WHERE email = 'user07321@example.com'";
        let plan = cold.query(&format!("EXPLAIN QUERY PLAN {query}")).await?;
        let rows = cold.query(query).await?;
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0][0].to_string(), "7321");
        assert_eq!(rows[0][1].to_string(), "500");
        let stats = store.stats();
        let parts_read = (stats.read_keys.iter())
            .filter(|k| k.starts_with(KV_PREFIX))
            .count();
        assert!(parts > 30);
        assert!(
            parts_read * 10 < parts,
            "cold lookup fetched too much of the database"
        );
        assert!(stats.bytes_read < stored_bytes as u64 / 10);

        println!("SQLite over an asynchronous in-memory KV store, without a cache");
        println!("Rows: 12000");
        println!("Stored bytes (including metadata): {stored_bytes}");
        println!(
            "Checkpoint: {} frames of {} pages, in {parts} parts of its pack",
            checkpoint.frames,
            state::FRAME_PAGES
        );
        println!("Cold open + query KV gets: {}", stats.gets);
        println!("Distinct pack parts fetched: {parts_read}");
        println!("KV bytes fetched: {}", stats.bytes_read);
        println!("Result: id={}, payload length={}", rows[0][0], rows[0][1]);
        for row in plan {
            println!(
                "Plan: {}",
                row.iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
                    .join(" | ")
            );
        }

        store.reset_stats();
        cold.query(query).await?;
        println!("Repeated warm lookup KV gets: {}", store.stats().gets);
        println!("PASS: fresh-engine indexed lookup fetched <10% of stored bytes.");
        Ok(())
    })
}
