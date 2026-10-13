// A Tenant virtual object per tenant, whose state is SQL: the audit log of
// the tenant's tool calls, and the calls held for approval.
//
// The gateway (gateway.ts) records each call that ended, and each call it
// holds. A manager lists the held ones (`pending`) and decides (`decide`),
// which resolves the awakeable the gateway waits on.
//
// Environment: PORT and ABORT_PER_MILLE (see ../faults.ts).
import * as restate from '@restatedev/restate-sdk';
import { serveIfMain } from '../faults.ts';

/** A tool call, recorded once it has ended; `call` is the gateway's invocation id. */
export interface CallRecord {
  call: string;
  user: string;
  tool: string;
  outcome: 'ok' | 'error' | 'denied';
  detail: string;
  ms: number;
}

/** A call held for approval, until `decide` resolves its awakeable. */
export interface Held {
  awakeable: string;
  user: string;
  tool: string;
  arguments: unknown;
}

const SCHEMA = `
  CREATE TABLE IF NOT EXISTS calls(call TEXT PRIMARY KEY, user TEXT, tool TEXT, outcome TEXT, detail TEXT, ms INTEGER);
  CREATE TABLE IF NOT EXISTS held(awakeable TEXT PRIMARY KEY, user TEXT, tool TEXT, arguments TEXT, approved INTEGER);`;

export const tenant = restate.object({
  name: 'Tenant',
  handlers: {
    record: (ctx: restate.SqlObjectContext, r: CallRecord) =>
      ctx.transaction(async ({ db }) => {
        await db.execute(SCHEMA);
        await db.execute('INSERT OR IGNORE INTO calls VALUES(?, ?, ?, ?, ?, ?)', [
          r.call,
          r.user,
          r.tool,
          r.outcome,
          r.detail,
          r.ms,
        ]);
      }),

    hold: (ctx: restate.SqlObjectContext, h: Held) =>
      ctx.transaction(async ({ db }) => {
        await db.execute(SCHEMA);
        await db.execute('INSERT OR IGNORE INTO held VALUES(?, ?, ?, ?, NULL)', [
          h.awakeable,
          h.user,
          h.tool,
          JSON.stringify(h.arguments),
        ]);
      }),

    /** The calls waiting for a decision. */
    pending: (ctx: restate.SqlObjectContext) =>
      ctx.transaction(async ({ db }): Promise<Held[]> => {
        await db.execute(SCHEMA);
        const rows = await db.query('SELECT awakeable, user, tool, arguments FROM held WHERE approved IS NULL');
        return rows.map(([awakeable, user, tool, args]) => ({
          awakeable: awakeable as string,
          user: user as string,
          tool: tool as string,
          arguments: JSON.parse(args as string) as unknown,
        }));
      }),

    /** Approve or deny a held call. The first decision stands; sent again, it resolves the awakeable again, harmlessly. */
    decide: async (
      ctx: restate.SqlObjectContext,
      { awakeable, approved }: { awakeable: string; approved: boolean },
    ) => {
      const decided = await ctx.transaction(async ({ db }) => {
        await db.execute(SCHEMA);
        await db.execute('UPDATE held SET approved = ? WHERE awakeable = ? AND approved IS NULL', [
          approved ? 1 : 0,
          awakeable,
        ]);
        const [row] = await db.query('SELECT approved FROM held WHERE awakeable = ?', [awakeable]);
        if (!row) throw new restate.TerminalError(`no call waits on ${awakeable}`);
        return row[0] === 1;
      });
      ctx.resolveAwakeable(awakeable, decided);
      return decided;
    },

    /** The tenant's calls, per user and tool. */
    report: (ctx: restate.SqlObjectContext) =>
      ctx.transaction(async ({ db }) => {
        await db.execute(SCHEMA);
        const rows = await db.query(
          `SELECT user, tool, count(*), sum(outcome = 'ok'), sum(outcome != 'ok'), cast(avg(ms) AS INTEGER)
             FROM calls GROUP BY user, tool ORDER BY user, tool`,
        );
        return rows.map(([user, tool, calls, ok, failed, avgMs]) => ({ user, tool, calls, ok, failed, avgMs }));
      }),
  },
  options: { ...restate.sqlOptions, retryPolicy: { initialInterval: 50, maxInterval: 500 } },
});

serveIfMain(import.meta, [tenant]);
