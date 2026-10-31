// The Restate calls of an invocation's tools (`call`, `signal`, `subagent`...
// of tools.ts), kept in the object's database (`restate_agent_calls`) so that
// a call is made once, however often its tool call runs.
//
// A crash or a suspension restarts the handler. pi, Claude and LangGraph then
// run a tool call that was cut off again, with its id. OpenCode and the
// DeepSeek Harness tell the model it was interrupted instead, and the model
// makes it again with a new id: made with the same tool and input, it is the
// call an earlier attempt made, so it takes that call's id, and so the same
// idempotency key: the same Restate call (a wait for a signal, a sub-agent's
// task), which it waits for again, or whose answer it gets.
//
// A call is kept until the framework's transcript has its result under the
// last id it was made with ({@link settle}, at each publish, and before each
// tool call): one that answered just before a crash, whose result the
// framework had not stored, is made again as the same call. An error there
// is not its result: OpenCode gives the call it interrupted one, and the
// model makes it again. Once stored, the model has its result, and an
// identical call it makes later is a call of its own.
import type * as restate from 'restate-objects';
import { Schema } from 'restate-objects/internal';
import { randomUUID } from 'node:crypto';

/** A tool's call: the tool, its input as JSON, the ids it was made with (the first is its own), and the attempt that last made it. */
interface Call {
  tool: string;
  input: string;
  ids: string[];
  attempt: string;
  /** Answered, until the transcript has its result. */
  done?: true;
}

/** The calls an invocation's tools made that the transcript has no result of yet. */
interface Calls {
  invocation: string;
  calls: Call[];
}

/** The table of the {@link Calls}: one row, of the invocation whose calls they are. */
const SCHEMA = new Schema('restate-agents.calls', [
  `CREATE TABLE restate_agent_calls (
    row INTEGER PRIMARY KEY CHECK (row = 0),
    invocation TEXT NOT NULL,
    calls TEXT NOT NULL
  )`,
]);

/** What the calls need of a handler's context. */
type CallsContext = Pick<restate.SessionContext, 'db' | 'request'>;

/** Store `calls`, committed before the call they record is made. */
async function store(ctx: CallsContext, { invocation, calls }: Calls) {
  await ctx.db.execute('INSERT OR REPLACE INTO restate_agent_calls (row, invocation, calls) VALUES (0, ?, ?)', [
    invocation,
    JSON.stringify(calls),
  ]);
}

/** The calls of `invocation`, as stored: none if the row is another invocation's. */
async function load(ctx: CallsContext, invocation: string): Promise<Calls> {
  await SCHEMA.ensure(ctx.db);
  const row = await ctx.db.get<{ invocation: string; calls: string }>(
    'SELECT invocation, calls FROM restate_agent_calls',
  );
  return row?.invocation === invocation
    ? { invocation, calls: JSON.parse(row.calls) as Call[] }
    : { invocation, calls: [] };
}

/** Each attempt's id, and its calls, by its context. */
const attempts = new WeakMap<object, { id: string; calls: Promise<Calls> }>();

/** The attempt of `ctx`: its id, and the calls of its invocation. */
function attemptOf(ctx: CallsContext) {
  let attempt = attempts.get(ctx);
  if (!attempt) {
    const calls = load(ctx, ctx.request.id);
    attempt = { id: randomUUID(), calls };
    attempts.set(ctx, attempt);
  }
  return attempt;
}

/**
 * The call of tool `tool` with `input` that tool call `callId` makes: the
 * one it made before (a run again), or the one an earlier attempt made with
 * the same tool and input (made again under a new id), or else a new one.
 * `run(id)` makes it with its own id, and stores that it answered.
 */
export async function callOf(ctx: CallsContext, tool: string, input: unknown, callId: string) {
  const attempt = attemptOf(ctx);
  const state = await attempt.calls;
  const json = JSON.stringify(input);
  let call =
    state.calls.find((c) => c.ids.includes(callId)) ??
    state.calls.find((c) => c.attempt !== attempt.id && c.tool === tool && c.input === json);
  if (call) {
    call.attempt = attempt.id;
    if (!call.ids.includes(callId)) call.ids.push(callId);
  } else {
    state.calls.push((call = { tool, input: json, ids: [callId], attempt: attempt.id }));
  }
  await store(ctx, state);
  const made = call;
  return {
    id: made.ids[0]!,
    /** It answered (a terminal error included): kept until the transcript has its result. */
    async answered() {
      made.done = true;
      await store(ctx, state);
    },
  };
}

/**
 * Forget the calls that answered whose result the framework stored: those
 * of which `stored(id)` is true for the last id they were made with. Called
 * at each publish, and before each tool call, with the transcript as the
 * framework stored it: also in an attempt that made no call yet, which would
 * otherwise take one an earlier attempt made, and the model has the result
 * of.
 */
export async function settle(ctx: CallsContext, stored: (id: string) => boolean) {
  const state = await attemptOf(ctx).calls;
  const kept = state.calls.filter((c) => !c.done || !stored(c.ids.at(-1)!));
  if (kept.length === state.calls.length) return;
  state.calls = kept;
  await store(ctx, state);
}
