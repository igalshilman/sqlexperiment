use sqlite_lazy_kv::{MapLayout, Result, Session, StorageOptions, simulation::MemoryKv};
use std::sync::Arc;

fn main() -> Result<()> {
    futures::executor::block_on(async {
        let store = Arc::new(MemoryKv::default());
        let layout = match std::env::args().nth(1).as_deref() {
            None | Some("radix") => MapLayout::Radix,
            Some("inline") => MapLayout::InlineRoot,
            Some("overrides") => MapLayout::HeadOverrides,
            Some(_) => return Err("expected radix, inline, or overrides".into()),
        };
        let mut db = Session::open_with_options(
            store.clone(),
            StorageOptions {
                layout,
                ..StorageOptions::default()
            },
        )
        .await?;
        println!("Map layout: {layout:?}");
        db.execute("CREATE TABLE accounts (id INTEGER PRIMARY KEY, email TEXT NOT NULL, payload TEXT NOT NULL)").await?;
        db.execute("CREATE UNIQUE INDEX accounts_email ON accounts(email)")
            .await?;
        db.execute("WITH RECURSIVE n(x) AS (VALUES(1) UNION ALL SELECT x+1 FROM n WHERE x < 12000) INSERT INTO accounts SELECT x, printf('user%05d@example.com', x), printf('%0500d', x) FROM n").await?;
        let local = db.io_stats();
        assert_eq!(store.stats().puts, 0);
        db.save().await?; // checkpoint, then durable object puts and head publication
        let publication = store.stats();
        store.crash(&[]);
        println!(
            "Private WAL bytes written locally: {}",
            local.private_wal_bytes_written
        );
        println!("Backend puts before save: 0");
        println!(
            "Backend bytes written by save: {}",
            publication.bytes_written
        );
        println!("Durable publication puts: {}", publication.puts);

        let db_blocks = store.block_count();
        let stored_bytes = store.stored_bytes();
        store.reset_stats();
        // New engine, pager, file handles and I/O adapter; only KV bytes survive.
        let mut cold = Session::open(store.clone()).await?;
        let query =
            "SELECT id, length(payload) FROM accounts WHERE email = 'user07321@example.com'";
        let plan = cold.query(&format!("EXPLAIN QUERY PLAN {query}")).await?;
        let rows = cold.query(query).await?;
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0][0].to_string(), "7321");
        assert_eq!(rows[0][1].to_string(), "500");
        let stats = store.stats();
        let blocks_read = stats
            .read_keys
            .iter()
            .filter(|k| k.contains("/block/"))
            .count();
        assert!(db_blocks > 100);
        assert!(
            blocks_read * 10 < db_blocks,
            "cold lookup fetched too much of the database"
        );
        assert!(stats.bytes_read < stored_bytes as u64 / 10);

        println!("SQLite / asynchronous in-memory KV / no Restate or replay");
        println!("Rows: 12000");
        println!("Stored bytes (including metadata): {stored_bytes}");
        println!("Stored block objects: {db_blocks} (4096 bytes each)");
        println!("Cold open + query KV gets: {}", stats.gets);
        println!("Distinct blocks fetched: {blocks_read}");
        println!("KV bytes fetched: {}", stats.bytes_read);
        println!("Forced async KV yields: {}", stats.async_yields);
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
