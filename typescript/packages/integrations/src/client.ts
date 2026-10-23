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
import type { Status, Stopped, Synced, SyncRequest } from './object.ts';
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
  Synced,
  SyncRequest,
  Turn,
  Waiting,
  Watch,
};

/** The handlers of an agent (`agent()` of any framework), as Restate's client calls them. */
export interface Agent {
  /** Answer `message`. */
  chat(ctx: unknown, message: string): Promise<string>;
  /** The conversation, a page at a time, or what is new or changed since a cursor, also while a message is answered. */
  messages(ctx: unknown, request?: MessagesRequest): Promise<Messages>;
  /** The conversation since a cursor, the status and the revision; with `revision`, once there is a newer one. */
  sync(ctx: unknown, request?: SyncRequest): Promise<Synced>;
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
  /** The revision, once there is one newer than `revision`, or at the timeout. */
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
   * The messages new or changed since the last time, by when they changed: a
   * message may come again, changed (a tool call with its output, say), with
   * its `id`, which a UI keeps them by, and orders them by.
   */
  messages: ChatMessage[];
  /** Where this leaves off: `follow(..., { cursor })` carries on from here, later or elsewhere. */
  cursor: number;
}

/** Where {@link follow} starts, and what stops it. */
export interface FollowOptions {
  /** The messages new or changed since this cursor (a page's, or a `Followed`'s); by default, all of them. */
  cursor?: number;
  /** Following ends once it aborts. */
  signal?: AbortSignal;
  /** How long one wait for a change lasts at most, in seconds (default 50). */
  timeoutSeconds?: number;
}

/** Whether a retry may answer: a network failure, Restate's (5xx) or too many requests (429); not a 4xx. */
const transient = (error: unknown) =>
  !(error instanceof clients.HttpCallError) || error.status >= 500 || error.status === 429;

/**
 * One `sync` of `followed`, tried again a second later with the same
 * idempotency key until it answers, unless its error would last; undefined
 * once `signal` aborts.
 */
async function syncRetried(
  followed: clients.IngressClient<Agent>,
  request: SyncRequest,
  signal: AbortSignal | undefined,
): Promise<Synced | undefined> {
  const opts = clients.rpc.opts<SyncRequest, Synced>({ signal, idempotencyKey: crypto.randomUUID() });
  for (;;) {
    if (signal?.aborted) return undefined;
    try {
      return await followed.sync(request, opts);
    } catch (error) {
      if (signal?.aborted) return undefined;
      if (!transient(error)) throw error;
      console.warn('follow: retrying', error);
      await new Promise((resolve) => setTimeout(resolve, 1000));
    }
  }
}

/**
 * The conversation of the agent `definition` (such as `agent('assistant')`)
 * with key `key`, as it changes, through `ingress` (Restate's client): what
 * the agent is doing and the messages so far, then each change, until
 * `signal` aborts.
 *
 * Each change is one `sync`, which waits in Restate for the next. A network
 * failure (or Restate's, restarting) does not end it: it tries again, and a
 * sync it retries keeps its idempotency key, so it joins the one already
 * waiting.
 */
export async function* follow(
  ingress: clients.Ingress,
  definition: VirtualObjectDefinition<string, Agent>,
  key: string,
  { cursor = 0, signal, timeoutSeconds = 50 }: FollowOptions = {},
): AsyncGenerator<Followed, void, void> {
  const followed = ingress.objectClient(definition, key);
  let revision: number | undefined;
  for (;;) {
    const synced = await syncRetried(followed, { cursor, revision, timeoutSeconds }, signal);
    if (!synced) return;
    // Nothing changed (the wait timed out): wait again.
    if (synced.revision !== revision || synced.messages.length > 0) {
      yield { status: synced.status, messages: synced.messages, cursor: synced.cursor };
    }
    cursor = synced.cursor;
    // With more to read, at once; else once something changes.
    revision = synced.more ? undefined : synced.revision;
  }
}
