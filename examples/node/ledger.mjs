// A ledger virtual object: accounts and transfers in SQL, with no journal.
//
// Each transfer is recorded with its id in the same SQL transaction as the
// balances it moves, so a retried or repeated transfer moves money once.
// Environment: PORT (default 9080), ABORT_PER_MILLE (abort the process at
// random state requests, for crash tests; see faults.mjs) and
// SQLITE_KV_CACHE_FILE.
import * as restate from '@restatedev/restate-sdk';
import { serveIfMain } from '../faults.mjs';

export const ledger = restate.object({
  name: 'Ledger',
  handlers: {
    /** Create the tables, and open `count` accounts with `balance` each, once. */
    init: (ctx, [count, balance]) =>
      ctx.sql(async (db) => {
        await db.execute(
          `CREATE TABLE IF NOT EXISTS accounts(id INTEGER PRIMARY KEY, opening INTEGER, balance INTEGER);
           CREATE TABLE IF NOT EXISTS transfers(id TEXT PRIMARY KEY, src INTEGER, dst INTEGER, amount INTEGER);`,
        );
        for (let id = 0; id < count; id++) {
          await db.execute('INSERT OR IGNORE INTO accounts VALUES(?, ?, ?)', [id, balance, balance]);
        }
      }),

    /** Move money between accounts; false if this transfer id was applied before. */
    transfer: (ctx, t) =>
      ctx.sql(async (db) => {
        const seen = await db.query('SELECT 1 FROM transfers WHERE id = ?', [t.id]);
        if (seen.length > 0) return false;
        await db.execute('INSERT INTO transfers VALUES(?, ?, ?, ?)', [t.id, t.from, t.to, t.amount]);
        await db.execute('UPDATE accounts SET balance = balance - ?1 WHERE id = ?2', [t.amount, t.from]);
        await db.execute('UPDATE accounts SET balance = balance + ?1 WHERE id = ?2', [t.amount, t.to]);
        return true;
      }),

    /** Totals, and accounts that disagree with the recorded transfers. */
    audit: (ctx) =>
      ctx.sql(async (db) => {
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
      }),
  },
  options: {
    ...restate.sqlOptions,
    retryPolicy: { initialInterval: 50, maxInterval: 500 },
  },
});

serveIfMain(import.meta, [ledger]);
