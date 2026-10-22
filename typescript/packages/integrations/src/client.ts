// An agent for Restate's client (@restatedev/restate-sdk-clients), from a
// chat UI or any other code: the agent's definition as the client takes it,
// its name and the types of its handlers, and the types of what they answer;
// and `follow`, which follows its conversation as it changes. It imports
// Restate's client, and nothing of the agent's code or the SDK, so a web app
// bundles it as it is.
//
//   import * as clients from '@restatedev/restate-sdk-clients';
//   import { agent, follow } from 'restate-agents/client';
//
//   const restate = clients.connect({ url: 'http://localhost:8080' });
//   const alice = restate.objectClient(agent('assistant'), 'alice');
//   const { messages } = await alice.messages({ limit: 20 });
//   for await (const { status, messages } of follow(restate, agent('assistant'), 'alice')) { ... }
import * as clients from '@restatedev/restate-sdk-clients';
import type { VirtualObjectDefinition } from '@restatedev/restate-sdk-core';
import type { ChatMessage, Messages, MessagesRequest, Part, Turn } from './common.ts';
import type { Status, Stopped } from './object.ts';
import type { Notifications, Resolved, Signal, Steered, Waiting, Watch } from './signals.ts';

export type {
  ChatMessage,
  Messages,
  MessagesRequest,
  Notifications,
  Part,
  Resolved,
  Signal,
  Status,
  Steered,
  Stopped,
  Turn,
  Waiting,
  Watch,
};

/** The handlers of an agent (`agent()` of any framework), as Restate's client calls them. */
export interface Agent {
  /** Answer `message`. */
  chat(ctx: unknown, message: string): Promise<string>;
  /** The conversation, a page at a time, also while a message is answered. */
  messages(ctx: unknown, request?: MessagesRequest): Promise<Messages>;
  /** What the agent is doing: the message it answers, and what its tools wait for. */
  status(ctx: unknown): Promise<Status>;
  /** Stop the message it answers: cancel its invocation. */
  stop(ctx: unknown): Promise<Stopped>;
  /** Add `message` to the message it answers, at its next tool call (or answer it as a message of its own). */
  steer(ctx: unknown, message: string): Promise<Steered>;
  /** What its tools wait for. */
  pending(ctx: unknown): Promise<Waiting[]>;
  /** Resolve a wait: by its id, or the oldest for a name. */
  signal(ctx: unknown, signal: Signal): Promise<Resolved>;
  /** The revision of its conversation, which each change increases. */
  notifications(ctx: unknown): Promise<Notifications>;
  /** The revision, once it is past `afterRevision`, or at the timeout. */
  watch(ctx: unknown, watch: Watch): Promise<Notifications>;
}

/**
 * The agent `name`, as Restate's client takes it: its name, typed with the
 * handlers of an agent (or `H`, an object of your own's).
 */
export const agent = <H = Agent, P extends string = string>(name: P) => ({ name }) as VirtualObjectDefinition<P, H>;

/** What {@link follow} gives each time the conversation changes. */
export interface Followed {
  /** What the agent is doing now. */
  status: Status;
  /**
   * The messages new or changed since the last time, oldest first: a message
   * may come again, changed (a tool call with its output, say), with its
   * `id`, which a UI keeps them by.
   */
  messages: ChatMessage[];
}

/** Where {@link follow} starts, and what stops it. */
export interface FollowOptions {
  /** The messages after this one (its `id`); by default, all of them. */
  after?: number;
  /** Following ends once it aborts. */
  signal?: AbortSignal;
  /** How long one wait for a change lasts at most, in seconds (default 50). */
  timeoutSeconds?: number;
}

/** After `error`, wait a second to try again: false, to stop instead, once `signal` aborted. */
async function retrying(error: unknown, signal: AbortSignal | undefined): Promise<boolean> {
  if (signal?.aborted) return false;
  console.warn('follow: retrying', error);
  await new Promise((resolve) => setTimeout(resolve, 1000));
  return true;
}

/**
 * The conversation of the agent `definition` (such as `agent('assistant')`)
 * with key `key`, as it changes, through `ingress` (Restate's client): what
 * the agent is doing and the messages so far, then each change, until
 * `signal` aborts.
 *
 * It reads the revision (`notifications`), then the status and the messages
 * since the last it gave, then waits for a newer revision (`watch`), which
 * waits in Restate. A network failure does not end it: it tries again, and a
 * wait it retries keeps its idempotency key, so it joins the wait already
 * there.
 */
export async function* follow(
  ingress: clients.Ingress,
  definition: VirtualObjectDefinition<string, Agent>,
  key: string,
  { after = -1, signal, timeoutSeconds = 50 }: FollowOptions = {},
): AsyncGenerator<Followed, void, void> {
  const followed = ingress.objectClient(definition, key);
  const opts = <I, O>(idempotencyKey?: string) => clients.rpc.opts<I, O>({ signal, idempotencyKey });
  // The last message given: it is read again, as it may have changed.
  let last = after;
  for (;;) {
    if (signal?.aborted) return;
    let revision: number;
    try {
      // The revision first: a change after it ends the wait below.
      ({ revision } = await followed.notifications(opts()));
      const status = await followed.status(opts());
      const messages: ChatMessage[] = [];
      for (let more = true; more;) {
        const from = messages.at(-1)?.id ?? Math.max(last - 1, -1);
        const page = await followed.messages({ after: from, limit: 100 }, opts());
        messages.push(...page.messages);
        more = page.more;
      }
      last = messages.at(-1)?.id ?? last;
      yield { status, messages };
    } catch (error) {
      if (await retrying(error, signal)) continue;
      return;
    }
    // One wait, retried with the same idempotency key.
    const window = crypto.randomUUID();
    for (;;) {
      try {
        await followed.watch({ afterRevision: revision, timeoutSeconds }, opts(window));
        break;
      } catch (error) {
        if (!(await retrying(error, signal))) return;
      }
    }
  }
}
