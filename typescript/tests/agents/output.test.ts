// An agent's answer as data (`output`): the agent hands it in with
// `submit_answer`, and `chat` answers with it; one that does not is reminded
// once, then the invocation fails; a crash at any point still answers with it.
// On pi's chat, with a fake model (../fakes/pi.ts), and Claude's (the
// teller's client, ../fakes/claude.ts); the others share the same code
// (output.ts).
import * as restate from 'restate-fns';
import * as claude from 'restate-agents/claude';
import * as pi from 'restate-agents/pi';
import { describe, expect, expectTypeOf, it, vi } from 'vitest';
import { z } from 'zod';
import { teller } from '../fakes/claude.ts';
import { callTools, fakeProvider, say, textOf } from '../fakes/pi.ts';
import { conversation, type PromptHandler } from '../testing.ts';

const PLAN = { summary: 'A calculator', parts: ['parser', 'history'] };

// Asked to plan, it hands a plan in; asked lazily, only once reminded; asked
// to refuse, never.
vi.mock('@earendil-works/pi-ai/providers/openai', () => ({
  openaiProvider: () =>
    fakeProvider('openai', 'gpt-6-sol', ({ messages }) => {
      const last = messages.at(-1)!;
      if (last.role === 'toolResult') return say('Handed in.');
      const asked = textOf(last.content);
      if (asked === 'Plan it' || asked.startsWith('Hand in your answer')) {
        const first = textOf(messages.find((m) => m.role === 'user')!.content);
        if (first === 'Refuse') return say('No.');
        if (first === 'Plan it' || asked.startsWith('Hand in')) return callTools([['submit_answer', PLAN]]);
      }
      return say('A plan, in prose');
    }),
}));

vi.mock('@anthropic-ai/sdk', () => import('../fakes/claude.ts'));

const Plan = z.object({ summary: z.string(), parts: z.array(z.string()) });
const chat = pi.chat({ output: Plan }) as unknown as PromptHandler;

describe("an agent's output", () => {
  it('is what chat answers with: the value the agent handed in, not its text', async () => {
    expect(await conversation(chat, 'a').attempt('inv1', 'Plan it')).toEqual(PLAN);
  });

  it('is asked for once more if the agent ended its turn without it', async () => {
    expect(await conversation(chat, 'b').attempt('inv1', 'Plan lazily')).toEqual(PLAN);
  });

  it('fails the invocation if the agent never hands it in', async () => {
    const refused = conversation(chat, 'c').attempt('inv1', 'Refuse');
    await expect(refused).rejects.toBeInstanceOf(restate.TerminalError);
    await expect(refused).rejects.toThrow(/handed in no answer/);
  });

  it('survives a crash at any point: the retry answers with it, handed in again if need be', async () => {
    for (let crashAt = 1; ; crashAt++) {
      const c = conversation(chat, `crash-${crashAt}`);
      if ((await c.attempt('inv1', 'Plan it', crashAt)) !== 'crashed') break;
      expect(await c.attempt('inv1', 'Plan it')).toEqual(PLAN);
    }
  }, 60_000);

  it("is Claude's too", async () => {
    // It hands the plan in, then says it did.
    teller.reply = (messages) => {
      const done = messages.length > 1;
      const content = done
        ? [{ type: 'text', text: 'Handed in.' }]
        : [{ type: 'tool_use', id: 'toolu_1', name: 'submit_answer', input: PLAN }];
      return {
        id: `msg_${messages.length}`,
        type: 'message',
        role: 'assistant',
        model: 'teller',
        content,
        stop_reason: done ? 'end_turn' : 'tool_use',
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
