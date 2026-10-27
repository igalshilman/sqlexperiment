// An agent's answer as data (`output`): its reply, as JSON of the schema,
// is what `chat` answers with; a reply that is not is answered with what is
// wrong, once, then the invocation fails; a crash at any point still answers
// with it. On pi's chat, with a fake model (../fakes/pi.ts), and Claude's
// (the teller's client, ../fakes/claude.ts); the others share the same code
// (output.ts).
import * as restate from 'restate-fns';
import * as claude from 'restate-agents/claude';
import * as pi from 'restate-agents/pi';
import { describe, expect, expectTypeOf, it, vi } from 'vitest';
import { z } from 'zod';
import { teller } from '../fakes/claude.ts';
import { fakeProvider, say, textOf } from '../fakes/pi.ts';
import { conversation, type PromptHandler } from '../testing.ts';

const PLAN = { summary: 'A calculator', parts: ['parser', 'history'] };

// Asked to plan, it answers with the plan, as JSON; chattily, with a sentence
// around it; asked lazily, in prose, then in a code fence once told; asked to
// refuse, in prose always.
vi.mock('@earendil-works/pi-ai/providers/openai', () => ({
  openaiProvider: () =>
    fakeProvider('openai', 'gpt-6-sol', ({ messages }) => {
      const asked = textOf(messages.at(-1)!.content);
      const first = textOf(messages.find((m) => m.role === 'user')!.content);
      if (first === 'Plan it') return say(JSON.stringify(PLAN));
      if (first === 'Plan chattily') return say(`Done! Here is the plan:\n${JSON.stringify(PLAN)}\nAll set.`);
      if (first === 'Plan lazily' && asked.startsWith('Your reply is not')) {
        return say('```json\n' + JSON.stringify(PLAN) + '\n```');
      }
      if (first === 'Plan wrongly') return say(JSON.stringify({ summary: 1 }));
      return say('A plan, in prose');
    }),
}));

vi.mock('@anthropic-ai/sdk', () => import('../fakes/claude.ts'));

const Plan = z.object({ summary: z.string(), parts: z.array(z.string()) });
const chat = pi.chat({ output: Plan }) as unknown as PromptHandler;

describe("an agent's output", () => {
  it('is what chat answers with: the value of the reply, not its text', async () => {
    expect(await conversation(chat, 'a').attempt('inv1', 'Plan it')).toEqual(PLAN);
  });

  it('is taken from a reply with a sentence around it', async () => {
    expect(await conversation(chat, 'chatty').attempt('inv1', 'Plan chattily')).toEqual(PLAN);
  });

  it('is asked for once more, with what is wrong, if the reply is not it (a code fence is taken)', async () => {
    expect(await conversation(chat, 'b').attempt('inv1', 'Plan lazily')).toEqual(PLAN);
  });

  it('fails the invocation if the reply is never it', async () => {
    const refused = conversation(chat, 'c').attempt('inv1', 'Refuse');
    await expect(refused).rejects.toBeInstanceOf(restate.TerminalError);
    await expect(refused).rejects.toThrow(/not the answer asked for: it is not JSON: "A plan, in prose"/);
    await expect(conversation(chat, 'd').attempt('inv1', 'Plan wrongly')).rejects.toThrow(/summary/);
  });

  it('survives a crash at any point: the retry answers with the same reply', async () => {
    for (let crashAt = 1; ; crashAt++) {
      const c = conversation(chat, `crash-${crashAt}`);
      if ((await c.attempt('inv1', 'Plan it', crashAt)) !== 'crashed') break;
      expect(await c.attempt('inv1', 'Plan it')).toEqual(PLAN);
    }
  }, 60_000);

  it("is Claude's too", async () => {
    // It answers with the plan, as JSON.
    teller.reply = (messages) => {
      return {
        id: `msg_${messages.length}`,
        type: 'message',
        role: 'assistant',
        model: 'teller',
        content: [{ type: 'text', text: JSON.stringify(PLAN) }],
        stop_reason: 'end_turn',
        stop_sequence: null,
        usage: { input_tokens: 0, output_tokens: 0 },
      } as unknown as ReturnType<typeof teller.reply>;
    };
    const planner = claude.chat({ output: Plan }) as unknown as PromptHandler;
    expect(await conversation(planner, 'claude').attempt('inv1', 'Plan it')).toEqual(PLAN);
  });

  it('types what prompt answers: the output, or else text', () => {
    expectTypeOf<Awaited<ReturnType<typeof pi.prompt<typeof Plan>>>>().toEqualTypeOf<z.infer<typeof Plan>>();
    expectTypeOf<Awaited<ReturnType<typeof pi.prompt<undefined>>>>().toEqualTypeOf<string>();
  });

  it("types what an agent's chat answers its callers", () => {
    const planner = claude.agent({ name: 'planner', output: Plan });
    const writer = claude.agent({ name: 'writer' });
    const plan = (ctx: restate.Context) => ctx.objectClient(planner, 'k').chat('Plan it');
    const write = (ctx: restate.Context) => ctx.objectClient(writer, 'k').chat('Write it');
    expectTypeOf<Awaited<ReturnType<typeof plan>>>().toEqualTypeOf<z.infer<typeof Plan>>();
    expectTypeOf<Awaited<ReturnType<typeof write>>>().toEqualTypeOf<string>();
  });
});
