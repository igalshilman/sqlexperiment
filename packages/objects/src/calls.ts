// oxlint-disable eslint/no-underscore-dangle -- the SDK's names, in its definitions

// Calls and sends from a session object's handler: `ctx.rpc` and `ctx.send`,
// and a transaction's `tx.send`. A target is a definition (with its key for
// an object or a workflow), or a literal naming it, for one known only at run
// time. Options go with the target: a client's methods take only their input.
import type * as restate from '@restatedev/restate-sdk';
import { serde } from '@restatedev/restate-sdk';
import type {
  Duration,
  HandlerDescriptor,
  Serde,
  ServiceDefinition,
  VirtualObjectDefinition,
  WorkflowDefinition,
} from '@restatedev/restate-sdk-core';

/** The options of a call, which apply to every call its client makes. */
export interface CallOptions {
  /**
   * A key that is the same on every attempt, so that a new attempt gets the
   * first call's result. Without one, every attempt makes the call again,
   * including the one that resumes a suspension: a call that may wait long
   * wants a key. Restate scopes it to the target handler: two calls of the
   * same handler through one keyed client are one invocation, so make a
   * client per call when that is not what you mean.
   */
  idempotencyKey?: string;
  /** Headers of the call. */
  headers?: Record<string, string>;
}

/** The options of a send. */
export interface SendOptions extends CallOptions {
  /** How long after the send (for `tx.send`, after the commit) the target runs. */
  delay?: Duration | number;
}

/** A target known only at run time: untyped, and JSON unless serdes are given. */
export interface Target {
  service: string;
  /** The key of an object or a workflow. */
  key?: string;
  handler: string;
  input?: unknown;
  inputSerde?: Serde<unknown>;
}

/** A definition the clients take: a service, a virtual object or a workflow, as `restate` makes them. */
type Definition = { name: string };

// The handlers of a definition, of whichever kind: each kind's type only
// infers from its own definitions, and is `unknown` for the others.
type OfObject<D> = D extends VirtualObjectDefinition<string, infer M> ? M : unknown;
type OfService<D> = D extends ServiceDefinition<string, infer M> ? M : unknown;
type OfWorkflow<D> = D extends WorkflowDefinition<string, infer M> ? M : unknown;
type HandlersOf<D> =
  unknown extends OfObject<D> ? (unknown extends OfService<D> ? OfWorkflow<D> : OfService<D>) : OfObject<D>;

/** A client of `D`: each handler takes its input, if any, and returns its output. */
export type Client<D> = {
  [K in keyof HandlersOf<D>]: HandlersOf<D>[K] extends (ctx: never, ...input: infer I) => PromiseLike<infer O>
    ? (...input: I) => Promise<O>
    : never;
};

/** A send client of `D`: each handler takes its input, if any. */
export type SendClient<D> = {
  [K in keyof HandlersOf<D>]: HandlersOf<D>[K] extends (ctx: never, ...input: infer I) => unknown
    ? (...input: I) => void
    : never;
};

/**
 * Calls: `ctx.rpc(service)` or `ctx.rpc(object, key)` for a typed client, with
 * the options of its calls last; or `ctx.rpc({ service, key, handler, input })`
 * for a target known only at run time. An object or a workflow needs its key,
 * a service has none: the call fails at once otherwise.
 */
export interface Rpc {
  <D extends Definition>(target: D, options?: CallOptions): Client<D>;
  <D extends Definition>(target: D, key: string, options?: CallOptions): Client<D>;
  <O = unknown>(call: Target & CallOptions & { outputSerde?: Serde<O> }): Promise<O>;
}

/** Sends, as {@link Rpc}'s calls: each returns at once, with nothing. */
export interface Send {
  <D extends Definition>(target: D, options?: SendOptions): SendClient<D>;
  <D extends Definition>(target: D, key: string, options?: SendOptions): SendClient<D>;
  (send: Target & SendOptions): void;
}

/** What a definition holds at run time, beside its name. */
type Defined = {
  name: string;
  _kind?: 'service' | 'object' | 'workflow';
  _handlers?: Record<string, HandlerDescriptor>;
};

/** The target, key and options of a typed client's arguments. */
function clientOf(args: unknown[]): { def: Defined; key: string | undefined; options: SendOptions } {
  const [def, second, third] = args as [Defined, unknown, unknown];
  const key = typeof second === 'string' ? second : undefined;
  const options = ((typeof second === 'string' ? third : second) ?? {}) as SendOptions;
  const keyed = def._kind !== 'service';
  if (keyed && key === undefined) {
    throw new TypeError(
      `${def.name} is ${def._kind === 'workflow' ? 'a workflow' : 'an object'}: call it with its key`,
    );
  }
  if (!keyed && key !== undefined) throw new TypeError(`${def.name} is a service: call it without a key`);
  return { def, key, options };
}

/** Whether `value` is a literal target rather than a definition. */
const isLiteral = (value: unknown): value is Target =>
  typeof (value as Target).handler === 'string' && typeof (value as Target).service === 'string';

/** A client whose methods make `call` of each one, with its input. */
function proxy(call: (method: string, input: unknown) => unknown): never {
  return new Proxy(
    {},
    {
      get: (_, method) => (typeof method === 'string' ? (input?: unknown) => call(method, input) : undefined),
    },
  ) as never;
}

/** `ctx.rpc` over the SDK context `ctx`. */
export function rpcOf(ctx: Pick<restate.Context, 'genericCall'>): Rpc {
  return (...args: unknown[]) => {
    if (isLiteral(args[0])) {
      const { service, key, handler, input, inputSerde, outputSerde, idempotencyKey, headers } = args[0] as Target &
        CallOptions & { outputSerde?: Serde<unknown> };
      return ctx.genericCall({
        service,
        method: handler,
        key,
        parameter: input,
        inputSerde: inputSerde ?? serde.json,
        outputSerde: outputSerde ?? serde.json,
        idempotencyKey,
        headers,
      });
    }
    const { def, key, options } = clientOf(args);
    return proxy((method, input) =>
      ctx.genericCall({
        service: def.name,
        method,
        key,
        parameter: input,
        inputSerde: def._handlers?.[method]?._inputSerde ?? serde.json,
        outputSerde: def._handlers?.[method]?._outputSerde ?? serde.json,
        idempotencyKey: options.idempotencyKey,
        headers: options.headers,
      }),
    );
  };
}

/** A send over `send`: `ctx.send` makes each at once, `tx.send` records it for its commit. */
export function sendOf(send: (send: restate.GenericSend<unknown>) => void): Send {
  return ((...args: unknown[]) => {
    if (isLiteral(args[0])) {
      const { service, key, handler, input, inputSerde, idempotencyKey, headers, delay } = args[0] as Target &
        SendOptions;
      send({
        service,
        method: handler,
        key,
        parameter: input,
        inputSerde: inputSerde ?? serde.json,
        idempotencyKey,
        headers,
        delay,
      });
      return undefined;
    }
    const { def, key, options } = clientOf(args);
    return proxy((method, input) => {
      send({
        service: def.name,
        method,
        key,
        parameter: input,
        inputSerde: def._handlers?.[method]?._inputSerde ?? serde.json,
        idempotencyKey: options.idempotencyKey,
        headers: options.headers,
        delay: options.delay,
      });
    });
  }) as Send;
}
