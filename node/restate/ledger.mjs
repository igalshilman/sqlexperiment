// A ledger virtual object: accounts and transfers in SQL, with no journal.
//
// The Node.js port of restate/src/bin/ledger.rs, with the same handlers and
// environment: PORT (default 9080), LEDGER_ABORT_PER_MILLE (abort the process
// at random state requests, for crash tests) and SQLITE_KV_CACHE_FILE.
import * as restate from '@restatedev/restate-sdk';
import { sql, sqlOptions, objectStore } from './sql.mjs';

const abortPerMille = Number(process.env.LEDGER_ABORT_PER_MILLE ?? 0);
const maybeAbort = () => {
  if (Math.random() * 1000 < abortPerMille) {
    console.error('ledger: aborting at a state request');
    process.abort();
  }
};

/** The object's store, aborting before or after random requests. */
function withAborts(ctx) {
  const store = objectStore(ctx);
  if (abortPerMille === 0) return store;
  return {
    get: async (key) => {
      maybeAbort();
      return store.get(key);
    },
    set: (key, value) => {
      maybeAbort();
      store.set(key, value);
      maybeAbort();
    },
    clear: (key) => {
      maybeAbort();
      store.clear(key);
      maybeAbort();
    },
    sync: async () => {
      maybeAbort();
      return store.sync();
    },
  };
}

const ledger = restate.object({
  name: 'Ledger',
  handlers: {
    /** Create the tables, and open `count` accounts with `balance` each, once. */
    init: sql(
      async (ctx, db, [count, balance]) => {
        await db.execute(
          `CREATE TABLE IF NOT EXISTS accounts(id INTEGER PRIMARY KEY, opening INTEGER, balance INTEGER);
           CREATE TABLE IF NOT EXISTS transfers(id TEXT PRIMARY KEY, src INTEGER, dst INTEGER, amount INTEGER);`,
        );
        for (let id = 0; id < count; id++) {
          await db.execute('INSERT OR IGNORE INTO accounts VALUES(?, ?, ?)', [id, balance, balance]);
        }
      },
      { store: withAborts },
    ),

    /** Move money between accounts; false if this transfer id was applied before. */
    transfer: sql(
      async (ctx, db, t) => {
        const seen = await db.query('SELECT 1 FROM transfers WHERE id = ?', [t.id]);
        if (seen.length > 0) return false;
        await db.execute('INSERT INTO transfers VALUES(?, ?, ?, ?)', [t.id, t.from, t.to, t.amount]);
        await db.execute('UPDATE accounts SET balance = balance - ?1 WHERE id = ?2', [t.amount, t.from]);
        await db.execute('UPDATE accounts SET balance = balance + ?1 WHERE id = ?2', [t.amount, t.to]);
        return true;
      },
      { store: withAborts },
    ),

    /** Totals, and accounts that disagree with the recorded transfers. */
    audit: sql(
      async (ctx, db) => {
        const [[integrity]] = await db.query('PRAGMA integrity_check');
        if (integrity !== 'ok') throw new restate.TerminalError('integrity check failed');
        const [[accounts, total, transfers, inconsistent]] = await db.query(
          `SELECT (SELECT count(*) FROM accounts), (SELECT sum(balance) FROM accounts),
                  (SELECT count(*) FROM transfers),
                  (SELECT count(*) FROM accounts a WHERE a.balance != a.opening
                      - (SELECT coalesce(sum(amount), 0) FROM transfers WHERE src = a.id)
                      + (SELECT coalesce(sum(amount), 0) FROM transfers WHERE dst = a.id))`,
        );
        return { accounts, total, transfers, inconsistent };
      },
      { store: withAborts },
    ),
  },
  options: {
    ...sqlOptions,
    retryPolicy: { initialInterval: 50, maxInterval: 500 },
  },
});

restate.serve({ services: [ledger], port: Number(process.env.PORT ?? 9080) });
