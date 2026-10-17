// A wrong model, provider or API key: the agent fails at once, at its
// definition or with a terminal error, rather than retry forever while its
// caller waits. LangGraph's agent takes a chat model of its own, here one that
// fails as a provider would.
import * as restate from '@restatedev/restate-sdk';
import { chatHandler, tool } from 'restate-agents';
import * as deepseek from 'restate-agents/deepseek';
import * as langgraph from 'restate-agents/langgraph';
import * as opencode from 'restate-agents/opencode';
import * as pi from 'restate-agents/pi';
import { describe, expect, it } from 'vitest';
import { z } from 'zod';
import { Teller } from '../fakes/langgraph.ts';
import { conversation } from '../testing.ts';

/** The Anthropic SDK's error of a wrong API key. */
class AuthenticationError extends Error {
  status = 401;
}

/** A chat model whose every request fails with `error`. */
class Failing extends Teller {
  #error: Error;
  constructor(error: Error) {
    super({});
    this.#error = error;
  }
  override reply(): never {
    throw this.#error;
  }
}

const answer = (model: Teller) => conversation(langgraph.chat({ model }), 'k').attempt('inv1', 'Hi');

describe('a wrong model', () => {
  it('that is not provider/model-id fails at the definition', () => {
    expect(() => pi.chat({ model: 'claude' })).toThrow(/not provider\/model-id/);
    expect(() => opencode.chat({ model: 'claude' })).toThrow(/not provider\/model-id/);
  });

  it("of another provider fails at the definition, for an agent of one provider's models", () => {
    expect(() => deepseek.chat({ model: 'anthropic/claude-sonnet-5-5' })).toThrow(/not deepseek's/);
    expect(() => langgraph.chat({ model: 'openai/gpt-6-luna' })).toThrow(/not anthropic's/);
  });

  it('that the provider refuses for good fails the invocation, saying what to check', async () => {
    const refused = answer(new Failing(new AuthenticationError('invalid x-api-key')));
    await expect(refused).rejects.toBeInstanceOf(restate.TerminalError);
    await expect(refused).rejects.toThrow(/check the agent's model .* API key.*invalid x-api-key/);
    await expect(answer(new Failing(new Error('Anthropic API key not found')))).rejects.toBeInstanceOf(
      restate.TerminalError,
    );
  });

  it('is told apart from errors that a retry may fix', async () => {
    const overloaded = answer(new Failing(new Error('529 overloaded')));
    await expect(overloaded).rejects.toThrow('529 overloaded');
    await expect(overloaded).rejects.not.toBeInstanceOf(restate.TerminalError);
  });
});

/** A tool's error named as the provider's (an HTTP library's 404, say). */
class NotFoundError extends Error {}

describe("a tool's error", () => {
  it('is never taken for the provider refusing the model', async () => {
    const lookup = tool({
      name: 'lookup',
      description: 'Looks a page up.',
      input: z.object({}),
      run: async () => {
        throw new NotFoundError('no such page');
      },
    });
    const failed = conversation(
      chatHandler(async (ctx) => lookup.run({}, { ctx, callId: 'call_1' })),
      'k',
    ).attempt('inv1', 'Hi');
    await expect(failed).rejects.toThrow('the tool lookup failed: no such page');
    await expect(failed).rejects.not.toBeInstanceOf(restate.TerminalError);
  });
});
