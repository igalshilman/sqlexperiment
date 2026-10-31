// The Restate calls of an invocation's tools (`call`, `signal`, `subagent`...
// of tools.ts), kept in the object's state so that a call is made once,
// however often its tool call runs.
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

/** The state key of the {@link Calls}. */
const CALLS = 'restate-agents.calls';

/** Each attempt's id, and its calls, by its context. */
const attempts = new WeakMap<object, { id: string; calls: Promise<Calls> }>();

/** The attempt of `ctx`: its id, and the calls of its invocation. */
function attemptOf(ctx: restate.ObjectContext) {
  let attempt = attempts.get(ctx);
  if (!attempt) {
    const invocation = ctx.request().id;
    const calls = ctx.get<Calls>(CALLS).then((c) => (c?.invocation === invocation ? c : { invocation, calls: [] }));
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
export async function callOf(ctx: restate.ObjectContext, tool: string, input: unknown, callId: string) {
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
  ctx.set(CALLS, state);
  const made = call;
  return {
    id: made.ids[0]!,
    /** It answered (a terminal error included): kept until the transcript has its result. */
    answered() {
      made.done = true;
      ctx.set(CALLS, state);
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
export async function settle(ctx: restate.ObjectContext, stored: (id: string) => boolean) {
  const state = await attemptOf(ctx).calls;
  const kept = state.calls.filter((c) => !c.done || !stored(c.ids.at(-1)!));
  if (kept.length === state.calls.length) return;
  state.calls = kept;
  ctx.set(CALLS, state);
}
