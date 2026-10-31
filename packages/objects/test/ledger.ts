// A ledger virtual object: accounts and transfers in SQL, with no journal.
//
// Each transfer is recorded with its id in the same SQL transaction as the
// balances it moves, so a retried or repeated transfer moves money once.
// Environment: PORT (default 9080), ABORT_PER_MILLE (abort the process at
// random state requests, for crash tests; see faults.ts) and
// RESTATE_SQL_CACHE_FILE.
import * as restate from 'restate-objects';
import { serveIfMain } from './faults.ts';
import { row } from './sql.ts';

/** A transfer of `amount` from account `from` to account `to`, by its unique `id`. */
export interface Transfer {
  id: string;
  from: number;
  to: number;
  amount: number;
}

/** The books' totals, and the number of accounts that disagree with the transfers. */
export interface Audit {
  accounts: number;
  total: number;
  transfers: number;
  inconsistent: number;
}

/** The ledger's handlers. */
export const handlers = {
  /** Create the tables, and open `count` accounts with `balance` each, once. */
  init: (ctx: restate.SessionContext, [count, balance]: [number, number]) =>
    ctx.db.transaction(async (db) => {
      await db.execute(
        `CREATE TABLE IF NOT EXISTS accounts(id INTEGER PRIMARY KEY, opening INTEGER, balance INTEGER);
           CREATE TABLE IF NOT EXISTS transfers(id TEXT PRIMARY KEY, src INTEGER, dst INTEGER, amount INTEGER);`,
      );
      for (let id = 0; id < count; id++) {
        await db.execute('INSERT OR IGNORE INTO accounts VALUES(?, ?, ?)', [id, balance, balance]);
      }
    }),

  /**
   * Move money between two accounts, once per transfer id, and return the
   * transfer as applied: a transfer id applied before returns the stored
   * transfer, whatever this request says, so the caller reports what moved.
   * A transfer that would create or destroy money is refused: an account that
   * does not exist, or an amount that is not a positive integer.
   */
  transfer: (ctx: restate.SessionContext, t: Transfer) =>
    ctx.db.transaction(async (db): Promise<Transfer> => {
      const [seen] = await db.values('SELECT src, dst, amount FROM transfers WHERE id = ?', [t.id]);
      if (seen) {
        const [from, to, amount] = seen as [number, number, number];
        return { id: t.id, from, to, amount };
      }
      if (!Number.isSafeInteger(t.amount) || t.amount <= 0) {
        throw new restate.TerminalError(`transfer ${t.id}: the amount ${t.amount} is not a positive integer`);
      }
      if (t.from === t.to) throw new restate.TerminalError(`transfer ${t.id}: from and to are one account`);
      const [accounts] = await row<[number]>(db, 'SELECT count(*) FROM accounts WHERE id IN (?, ?)', [t.from, t.to]);
      if (accounts !== 2) throw new restate.TerminalError(`transfer ${t.id}: no account ${t.from} or ${t.to}`);
      await db.execute('INSERT INTO transfers VALUES(?, ?, ?, ?)', [t.id, t.from, t.to, t.amount]);
      await db.execute('UPDATE accounts SET balance = balance - ?1 WHERE id = ?2', [t.amount, t.from]);
      await db.execute('UPDATE accounts SET balance = balance + ?1 WHERE id = ?2', [t.amount, t.to]);
      return { id: t.id, from: t.from, to: t.to, amount: t.amount };
    }),

  /** Totals, and accounts that disagree with the recorded transfers. */
  audit: (ctx: restate.SessionContext) =>
    ctx.db.transaction(async (db): Promise<Audit> => {
      const [integrity] = await row<[string]>(db, 'PRAGMA integrity_check');
      if (integrity !== 'ok') throw new restate.TerminalError('integrity check failed');
      const [accounts, total, transfers, inconsistent] = await row<[number, number, number, number]>(
        db,
        `SELECT (SELECT count(*) FROM accounts), (SELECT sum(balance) FROM accounts),
                  (SELECT count(*) FROM transfers),
                  (SELECT count(*) FROM accounts a WHERE a.balance != a.opening
                      - (SELECT coalesce(sum(amount), 0) FROM transfers WHERE src = a.id)
                      + (SELECT coalesce(sum(amount), 0) FROM transfers WHERE dst = a.id))`,
      );
      return { accounts, total, transfers, inconsistent };
    }),
};

export const ledger = restate.sessionObject({
  name: 'Ledger',
  handlers,
  options: {
    // Without end: the server pauses an invocation after its default attempts.
    retryPolicy: { initialInterval: 50, maxInterval: 500, maxAttempts: 100_000 },
  },
});

serveIfMain(import.meta, [ledger]);
