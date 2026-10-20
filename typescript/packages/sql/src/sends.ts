// oxlint-disable eslint/no-underscore-dangle -- the SDK's names, in its definitions

// The send clients of a transaction (`tx` in `ctx.transaction`), as the
// context's: each send is recorded, to be made once the transaction commits.
import type * as restate from '@restatedev/restate-sdk';
import { TerminalError } from '@restatedev/restate-sdk';
import type { HandlerDescriptor, Serde } from '@restatedev/restate-sdk-core';
import type { SqlTransactionSends } from './session.ts';

/** A service, object or workflow definition, as the send clients take it. */
type ClientTarget = { name: string; _kind?: string; _handlers?: Record<string, HandlerDescriptor> };

// What a recorded send returns: there is no invocation yet.
const NO_HANDLE: restate.InvocationHandle = Object.defineProperty({} as restate.InvocationHandle, 'invocationId', {
  get() {
    throw new TerminalError('a send of ctx.transaction has no invocation id: it is made once the transaction commits');
  },
});

/** The options of a send: `restate.rpc.sendOpts(...)`'s. */
const optionsOf = (value: unknown): restate.ClientSendOptions<unknown> | undefined =>
  typeof (value as { getOpts?: unknown } | undefined)?.getOpts === 'function'
    ? (value as { getOpts(): restate.ClientSendOptions<unknown> }).getOpts()
    : undefined;

/** Send clients that record each send with `record`, as the context's send clients make them. */
// oxlint-disable-next-line typescript/no-explicit-any
export function sendClients(defaultSerde: Serde<any>, record: (send: restate.GenericSend<unknown>) => void) {
  const genericSend = (send: restate.GenericSend<unknown>) => {
    record(send);
    return NO_HANDLE;
  };
  const client = ({ name, _handlers }: ClientTarget, key?: string): never =>
    new Proxy(
      {},
      {
        get:
          (_, method: string) =>
          (...args: unknown[]) => {
            // (parameter), (options), or (parameter, options).
            const last = optionsOf(args.at(-1));
            const opts = last ?? {};
            const parameter = last && args.length === 1 ? undefined : args[0];
            return genericSend({
              service: name,
              method,
              parameter,
              key,
              headers: opts.headers,
              delay: opts.delay,
              inputSerde: opts.input ?? _handlers?.[method]?._inputSerde ?? defaultSerde,
              idempotencyKey: opts.idempotencyKey,
              limitKey: opts.limitKey,
              name: opts.name,
            });
          },
      },
    ) as never;
  const sends: Record<keyof SqlTransactionSends, unknown> = {
    serviceSendClient: (def: ClientTarget) => client(def),
    objectSendClient: (def: ClientTarget, key: string) => client(def, key),
    workflowSendClient: (def: ClientTarget, key: string) => client(def, key),
    sendClient: (def: ClientTarget, key?: string) => client(def, def._kind === 'service' ? undefined : key),
    genericSend,
  };
  return sends as SqlTransactionSends;
}
