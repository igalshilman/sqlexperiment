// pi's Harness configured as it runs, as examples/harness.ts does, without a
// Restate server: each user's model and persona from its database, and the
// conversation compacted and reset.
import type * as restate from 'restate-session-objects';
import { tool } from 'restate-agents';
import * as pi from 'restate-agents/pi';
import { describe, expect, it, vi } from 'vitest';
import { z } from 'zod';
import { callTools, fakeProvider, requests, say, textOf, type Respond } from './fakes/pi.ts';
import { type Entry, handlerOf, sqlAttempt } from './support.ts';

// Each model says who it is and what it was told so far; asked the time, it
// calls the clock first.
const respond =
  (model: string): Respond =>
  ({ messages }) => {
    const last = messages.at(-1)!;
    if (last.role === 'toolResult') return say(`${model}: the time is ${textOf(last.content)}`);
    if (textOf(last.content).includes('time')) return callTools([['clock', {}]]);
    const system = messages.find((m) => m.role === 'system') as { sections?: Record<string, string> } | undefined;
    const said = messages.filter((m) => m.role === 'user').map((m) => textOf(m.content));
    const persona = Object.values(system?.sections ?? {})
      .join(' ')
      .replaceAll(/<\/?system>/g, '')
      .trim();
    return say(`${model} (${persona}): ${said.join(' / ')}`);
  };
vi.mock('@earendil-works/pi-ai/providers/all', () => ({
  builtinProviders: () => [
    fakeProvider('openai', 'gpt-6-sol', respond('gpt')),
    fakeProvider('anthropic', 'claude-sonnet-5-5', respond('claude')),
  ],
}));

interface Settings {
  model?: string;
  persona?: string;
}
const SCHEMA = 'CREATE TABLE IF NOT EXISTS settings (id INTEGER PRIMARY KEY CHECK (id = 0), json TEXT NOT NULL)';
async function settingsOf(ctx: restate.SessionContext): Promise<Settings> {
  await ctx.db.execute(SCHEMA);
  const [row] = await ctx.db.values('SELECT json FROM settings');
  return row ? (JSON.parse(String(row[0])) as Settings) : {};
}
const clock = tool({ name: 'clock', description: 'The time.', input: z.object({}), run: async () => '12:00' });
const options = async (ctx: restate.SessionContext): Promise<pi.Options> => {
  const { model, persona } = await settingsOf(ctx);
  return { model, system: persona ?? 'You are a helpful assistant.', tools: [clock] };
};

// The handlers of examples/harness.ts.
const chat = pi.chat(options);
const configure = async (ctx: restate.SessionContext, settings: Settings) => {
  const next = { ...(await settingsOf(ctx)), ...settings };
  await ctx.db.execute('INSERT OR REPLACE INTO settings (id, json) VALUES (0, ?)', [JSON.stringify(next)]);
  return next;
};
const compact = async (ctx: restate.SessionContext, instructions?: string) =>
  pi.withPi(ctx, await options(ctx), async ({ conversation, context }) => {
    const root = await conversation();
    await root.compact(instructions || undefined, context);
    await root.waitForIdle(context);
  });
const reset = async (ctx: restate.SessionContext, handoff?: string) =>
  pi.withPi(ctx, await options(ctx), async ({ conversation, context }) =>
    (await conversation()).reset(handoff || undefined, context),
  );

/** The object with key `key`: its database lives across invocations. */
function store(key: string) {
  const map = new Map<string, Uint8Array>();
  return <I, O>(handler: Entry<(ctx: restate.SessionContext, input: I) => Promise<O>>, id: string, input: I) => {
    const sql = sqlAttempt({ map, key, id });
    return sql.end(() => handlerOf(handler)(sql.ctx, input));
  };
}

describe('a Harness configured as it runs', () => {
  it('answers on each user’s model and persona, with the tools', async () => {
    const alice = store('alice');
    const bob = store('bob');
    expect(await alice(chat, 'a1', 'Hi')).toBe('gpt (You are a helpful assistant.): Hi');
    await bob(configure, 'b0', { model: 'anthropic/claude-sonnet-5-5', persona: 'You are a pirate.' });
    expect(await bob(chat, 'b1', 'Hi')).toBe('claude (You are a pirate.): Hi');
    expect(await bob(chat, 'b2', 'What time is it?')).toBe('claude: the time is 12:00');
    // The same conversation, on another model.
    await alice(configure, 'a2', { model: 'anthropic/claude-sonnet-5-5' });
    expect(await alice(chat, 'a3', 'Again')).toBe('claude (You are a helpful assistant.): Hi / Again');
  });

  it('asks OpenAI for the service tier it is given (fast mode)', async () => {
    const carol = store('carol');
    requests.length = 0;
    await carol(pi.chat({ serviceTier: 'fast' }), 'c1', 'Hi');
    await store('dave')(pi.chat(), 'd1', 'Hi');
    expect(requests.map((r) => r.serviceTier)).toEqual(['fast', undefined]);
  });

  it('compacts the conversation, and starts it over with a handoff', async () => {
    const alice = store('alice');
    await alice(chat, 'a1', 'I am Alice');
    await alice(compact, 'a2', 'Keep the names');
    expect(await alice(chat, 'a3', 'Who am I?')).toMatch(/: .*Who am I\?$/);
    // After a reset, the model has the handoff and what comes next.
    await alice(reset, 'a4', 'Alice likes short answers');
    expect(await alice(chat, 'a5', 'Hi')).toBe('gpt (You are a helpful assistant.): Alice likes short answers / Hi');
    // The UI still has all of it.
    const { messages } = await alice(pi.messages(), 'read', {});
    expect(messages.map((m) => m.role)).toEqual([
      'user',
      'assistant',
      'user',
      'assistant',
      'user',
      'user',
      'assistant',
    ]);
  });
});
