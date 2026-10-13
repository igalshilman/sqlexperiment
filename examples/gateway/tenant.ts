// A Tenant virtual object per tenant, whose state is SQL: the gateway's audit
// log of the tenant's tool calls, and the approvals they wait for.
//
// The gateway (gateway.ts) sends it a record of each call, and of each call
// held for approval. A manager decides with `decide`, which resolves the
// awakeable the gateway waits on. The log is a table to query: `report` is
// what the tenant's tools did, how often, how fast, and what was refused.
//
// Environment: PORT and ABORT_PER_MILLE (see ../faults.ts).
import * as restate from '@restatedev/restate-sdk';
import { serveIfMain } from '../faults.ts';

/** What became of a tool call. */
export type Outcome = 'ok' | 'error' | 'denied' | 'rejected';

/** A tool call, as the gateway records it once it has ended. */
export interface CallRecord {
  /** The gateway's invocation id: one record per call, however often it is sent. */
  call: string;
  at: number;
  caller: string;
  tool: string;
  arguments: unknown;
  outcome: Outcome;
  detail: string;
  ms: number;
}

/** A call held for approval, until `decide` resolves its awakeable. */
export interface Approval {
  awakeable: string;
  call: string;
  at: number;
  caller: string;
  tool: string;
  arguments: unknown;
}

/** A manager's decision on a held call. */
export interface Decision {
  approved: boolean;
  by: string;
}

const SCHEMA = `
  CREATE TABLE IF NOT EXISTS calls(call TEXT PRIMARY KEY, at INTEGER, caller TEXT, tool TEXT,
    arguments TEXT, outcome TEXT, detail TEXT, ms INTEGER);
  CREATE TABLE IF NOT EXISTS approvals(awakeable TEXT PRIMARY KEY, call TEXT, at INTEGER, caller TEXT,
    tool TEXT, arguments TEXT, approved INTEGER, by TEXT);`;

export const tenant = restate.object({
  name: 'Tenant',
  handlers: {
    /** Record a call that ended, once per call. */
    record: (ctx: restate.SqlObjectContext, r: CallRecord) =>
      ctx.transaction(async ({ db }) => {
        await db.execute(SCHEMA);
        await db.execute('INSERT OR IGNORE INTO calls VALUES(?, ?, ?, ?, ?, ?, ?, ?)', [
          r.call,
          r.at,
          r.caller,
          r.tool,
          JSON.stringify(r.arguments),
          r.outcome,
          r.detail,
          r.ms,
        ]);
      }),

    /** Record a call held for approval. */
    hold: (ctx: restate.SqlObjectContext, a: Approval) =>
      ctx.transaction(async ({ db }) => {
        await db.execute(SCHEMA);
        await db.execute('INSERT OR IGNORE INTO approvals VALUES(?, ?, ?, ?, ?, ?, NULL, NULL)', [
          a.awakeable,
          a.call,
          a.at,
          a.caller,
          a.tool,
          JSON.stringify(a.arguments),
        ]);
      }),

    /** The calls waiting for a decision. */
    pending: (ctx: restate.SqlObjectContext) =>
      ctx.transaction(async ({ db }) => {
        await db.execute(SCHEMA);
        const rows = await db.query(
          'SELECT awakeable, call, at, caller, tool, arguments FROM approvals WHERE approved IS NULL ORDER BY at',
        );
        return rows.map(([awakeable, call, at, caller, tool, args]) => ({
          awakeable,
          call,
          at,
          caller,
          tool,
          arguments: JSON.parse(args as string) as unknown,
        })) as Approval[];
      }),

    /**
     * Decide on a held call, and let it go on. The first decision stands: a
     * second one, or this one sent again, resolves the awakeable with it.
     */
    decide: async (ctx: restate.SqlObjectContext, { awakeable, approved, by }: { awakeable: string } & Decision) => {
      const decision = await ctx.transaction(async ({ db }): Promise<Decision> => {
        await db.execute(SCHEMA);
        const [held] = await db.query('SELECT approved, by FROM approvals WHERE awakeable = ?', [awakeable]);
        if (!held) throw new restate.TerminalError(`no call waits on ${awakeable}`);
        if (held[0] !== null) return { approved: held[0] === 1, by: held[1] as string };
        await db.execute('UPDATE approvals SET approved = ?, by = ? WHERE awakeable = ?', [
          approved ? 1 : 0,
          by,
          awakeable,
        ]);
        return { approved, by };
      });
      // Outside the transaction: the decision is stored, and resolving again is harmless.
      ctx.resolveAwakeable(awakeable, decision);
      return decision;
    },

    /** What the tenant's tools did: per tool, and the last calls. */
    report: (ctx: restate.SqlObjectContext) =>
      ctx.transaction(async ({ db }) => {
        await db.execute(SCHEMA);
        const tools = await db.query(
          `SELECT tool, count(*), sum(outcome = 'ok'), sum(outcome = 'error'), sum(outcome IN ('denied', 'rejected')),
                  cast(avg(ms) AS INTEGER), max(ms)
             FROM calls GROUP BY tool ORDER BY tool`,
        );
        const last = await db.query(
          'SELECT tool, caller, outcome, ms, detail FROM calls ORDER BY at DESC, call LIMIT 10',
        );
        return {
          tools: tools.map(([tool, calls, ok, errors, refused, avgMs, maxMs]) => ({
            tool,
            calls,
            ok,
            errors,
            refused,
            avgMs,
            maxMs,
          })),
          last: last.map(([tool, caller, outcome, ms, detail]) => ({ tool, caller, outcome, ms, detail })),
        };
      }),
  },
  options: {
    ...restate.sqlOptions,
    retryPolicy: { initialInterval: 50, maxInterval: 500 },
  },
});

serveIfMain(import.meta, [tenant]);
