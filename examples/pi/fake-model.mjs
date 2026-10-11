// A fake model for pi-ai: a provider whose replies a function computes from
// the conversation, so an agent runs without an API key, deterministically.
//
//   const models = createModels();
//   models.setProvider(fakeProvider('echo', (context) => say(lastText(context))));
//
// `respond(context)` gets each model call's context (its `messages`) and
// returns the assistant message, built with `say`, `callTools` or pi-ai's faux
// helpers. Every model call is answered, however many there are, which
// matters when a retried attempt makes a model call again.
import { fauxAssistantMessage, fauxProvider, fauxText, fauxToolCall } from '@earendil-works/pi-ai/providers/faux';

/** A pi-ai provider `name`, with one model of the same id, answering with `respond`. */
export function fakeProvider(name, respond) {
  const faux = fauxProvider({ provider: name, models: [{ id: name }] });
  // The faux provider answers from a queue: each reply queues the next.
  const reply = (context) => {
    faux.appendResponses([reply]);
    return respond(context);
  };
  faux.setResponses([reply]);
  return faux.provider;
}

/** The text of a message's content. */
export const textOf = (content) =>
  typeof content === 'string'
    ? content
    : content
        .filter((c) => c.type === 'text')
        .map((c) => c.text)
        .join('');

/** The text of the conversation's last message. */
export const lastText = (context) => textOf(context.messages.at(-1).content);

/** A reply in text. */
export const say = (text) => fauxAssistantMessage(fauxText(text));

/** A reply that calls tools: `calls` is a list of [name, arguments], after an optional text. */
export const callTools = (calls, text) =>
  fauxAssistantMessage(
    [...(text ? [fauxText(text)] : []), ...calls.map(([name, args]) => fauxToolCall(name, args))],
    { stopReason: 'toolUse' },
  );
