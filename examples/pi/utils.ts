// Models for the agent example: a fake model for pi-ai, the scripted teller
// built on it, and `modelFromEnv`, which picks a real model or the teller.
//
// The fake model is a provider whose replies a function computes from the
// conversation, so an agent runs without an API key, deterministically.
//
//   const models = createModels();
//   models.setProvider(fakeProvider('echo', (context) => say(lastText(context))));
//
// `respond(context)` gets each model call's context (its `messages`) and
// returns the assistant message, built with `say`, `callTools` or pi-ai's faux
// helpers. Every model call is answered, however many there are, which
// matters when a retried attempt makes a model call again.
import type {
  AssistantMessage,
  Message,
  MutableModels,
  Provider,
  ToolCall,
  TranscriptContext,
} from '@earendil-works/pi-ai';
import { createModels } from '@earendil-works/pi-ai/models';
import { fauxAssistantMessage, fauxProvider, fauxText, fauxToolCall } from '@earendil-works/pi-ai/providers/faux';

/** A model's reply to the conversation in `context`. */
export type Respond = (context: TranscriptContext) => AssistantMessage;

/** A pi-ai provider `name`, with one model of the same id, answering with `respond`. */
export function fakeProvider(name: string, respond: Respond): Provider {
  const faux = fauxProvider({ provider: name, models: [{ id: name }] });
  // The faux provider answers from a queue: each reply queues the next.
  const reply = (context: TranscriptContext): AssistantMessage => {
    faux.appendResponses([reply]);
    return respond(context);
  };
  faux.setResponses([reply]);
  return faux.provider;
}

/** The text of a message's content. */
export const textOf = (content: Message['content']): string =>
  typeof content === 'string'
    ? content
    : content
        .filter((c) => c.type === 'text')
        .map((c) => c.text)
        .join('');

/** The text of the conversation's last message. */
export const lastText = (context: TranscriptContext) => textOf(context.messages.at(-1)!.content);

/** A reply in text. */
export const say = (text: string) => fauxAssistantMessage(fauxText(text));

/** A reply that calls tools: `calls` is a list of [name, arguments], after an optional text. */
export const callTools = (calls: [name: string, args: ToolCall['arguments']][], text?: string) =>
  fauxAssistantMessage([...(text ? [fauxText(text)] : []), ...calls.map(([name, args]) => fauxToolCall(name, args))], {
    stopReason: 'toolUse',
  });

/** The scripted teller: transfers and audits on request, and reports the tool's result. */
export const teller: Respond = (context) => {
  const text = lastText(context);
  if (context.messages.at(-1)!.role === 'toolResult') return say(`Done: ${text}`);
  const move = /move (\d+) from (\d+) to (\d+)/i.exec(text);
  if (move) {
    const [amount, from, to] = move.slice(1, 4).map(Number) as [number, number, number];
    return callTools([['transfer', { from, to, amount }]], 'On it.');
  }
  if (/audit/i.test(text)) return callTools([['audit', {}]]);
  return say('Ask me to "move 5 from 1 to 2", or to "audit" the books.');
};

/**
 * `{ models, model }` for withPi: PI_MODEL=provider/model-id (such as
 * anthropic/claude-sonnet-4-5, with the provider's API key), or by default
 * the teller, which needs no key.
 */
export async function modelFromEnv(): Promise<{ models: MutableModels; model: string }> {
  const models = createModels();
  const model = process.env.PI_MODEL ?? 'teller/teller';
  const [provider] = model.split('/');
  if (provider === 'teller') {
    models.setProvider(fakeProvider('teller', teller));
  } else {
    // Such as anthropicProvider() from @earendil-works/pi-ai/providers/anthropic.
    const module = await import(`@earendil-works/pi-ai/providers/${provider}`);
    models.setProvider(module[`${provider}Provider`]());
  }
  return { models, model };
}
