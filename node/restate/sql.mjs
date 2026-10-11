// SQL in a Restate virtual object, with the TypeScript SDK and no journal
// replay: `sql(handler)` wraps an exclusive object handler, which then gets the
// object's database. See README.md.
import * as restate from '@restatedev/restate-sdk';
import sqliteKv from 'sqlite-kv';

/**
 * Options for a service whose handlers use `sql`: no journal replay, since a
 * later attempt makes different reads; lazy state, so an invocation does not
 * start by loading every page; and no journal kept after an invocation
 * completes, since it holds every page it read.
 */
export const sqlOptions = {
  journalReplay: false,
  enableLazyState: true,
  journalRetention: 0,
};

/**
 * The object's state as an ordered store (see the sqlite-kv addon).
 *
 * The SDK waits on one thing at a time per invocation, and the engine reads
 * pages concurrently. So each request is issued at once, which keeps the
 * commands in the engine's order with respect to `set` and `clear`, and the
 * requests issued together are awaited as one batch (`RestatePromise.allSettled`),
 * one batch after another.
 */
export function objectStore(ctx) {
  const waiting = [];
  let draining = false;
  const drain = async () => {
    while (waiting.length > 0) {
      const batch = waiting.splice(0);
      const results = await restate.RestatePromise.allSettled(batch.map((r) => r.promise));
      results.forEach((result, i) =>
        result.status === 'fulfilled' ? batch[i].resolve(result.value) : batch[i].reject(result.reason),
      );
    }
    draining = false;
  };
  const wait = (promise) =>
    new Promise((resolve, reject) => {
      waiting.push({ promise, resolve, reject });
      if (!draining) {
        draining = true;
        setImmediate(drain);
      }
    });
  return {
    get: (key) => wait(ctx.get(key, restate.serde.binary)),
    set: (key, value) => ctx.set(key, value, restate.serde.binary),
    clear: (key) => ctx.clear(key),
    // Acknowledged after the commands before it are stored.
    sync: () => wait(ctx.run('sync', () => {})),
  };
}

/**
 * Wrap `handler(ctx, db, input)` so that it runs as one SQL transaction
 * against the object's database: durable as a whole once the handler returns.
 * An SQL error, or the handler's own, is terminal, since a retry would repeat
 * it; a storage error ends the session and is retried.
 */
export function sql(handler, { store = objectStore } = {}) {
  return async (ctx, input) => {
    const namespace = `${ctx.request().target.service}/${ctx.key}`;
    const db = await sqliteKv.open(namespace, store(ctx));
    let result;
    try {
      result = await db.transaction((tx) => handler(ctx, tx, input));
    } catch (error) {
      if (!db.isUsable() || error instanceof restate.TerminalError) throw error;
      await db.save();
      throw new restate.TerminalError(String(error?.message ?? error));
    }
    // Confirms the session's writes (one ctx.run, if it wrote) for the cache.
    await db.save();
    return result;
  };
}
