// An agent's answer as data, with `output` (every framework's option): a zod
// object schema. The agent's reply is its answer, as JSON: the system prompt
// asks for it, with the schema, and each prompt again, at its end; the reply
// is checked against it. The
// prompt (and so `chat`) then answers with the value, as an RPC answers.
//
//   const planner = claude.agent({ name: 'planner', output: Plan });   // chat answers with a Plan
//
// The JSON may be in a code fence, or after a sentence. A reply that is not
// such JSON is answered with what is wrong with it, once, for the agent to
// answer again; then the prompt fails (a terminal error), saying how the
// reply starts. A retried attempt resumes the conversation, so it reads the
// same reply.
import * as restate from 'restate-session-objects';
import { z } from 'zod';

/** What a prompt answers: its text, or with `output`, the value of the agent's reply. */
export type Answer<O> = O extends z.ZodObject ? z.infer<O> : string;

/** What the system prompt asks for, with `output`: JSON of its schema, and nothing else. */
const format = (output: z.ZodObject) =>
  'Your final reply is your answer, as JSON only (no other text): an object of this JSON schema.\n' +
  JSON.stringify(z.toJSONSchema(output));

/**
 * What each prompt ends with, with `output`: the system prompt's ask, again,
 * where an agent reads it last (after a long run of tool calls, it may have
 * drifted from its system prompt to a summary in prose).
 */
const reminder =
  '\n\nWhen you are done, reply with your answer as JSON only, of the schema you were given: no other text.';

/**
 * The JSON `reply` holds: all of it, what a code fence (```json) holds, or
 * else its object, from its first `{` to its last `}` (a model that ends a
 * long run often says a sentence first: "Done. Here is the summary: {...}").
 */
function jsonOf(reply: string): unknown {
  const fenced = /```(?:json)?\s*\n([^]*?)\n?```/.exec(reply)?.[1];
  const object = reply.slice(reply.indexOf('{'), reply.lastIndexOf('}') + 1);
  for (const text of [reply, fenced, object]) {
    try {
      if (text?.trim()) return JSON.parse(text);
    } catch {
      // the next
    }
  }
  return undefined;
}

/** The start of `reply`, for an error. */
const excerpt = (reply: string) => (reply.length > 300 ? `${reply.slice(0, 300)}…` : reply);

/** `reply` as a value of `output`, or what is wrong with it. */
function parsed<O extends z.ZodObject>(output: O, reply: string): { value: z.infer<O> } | { wrong: string } {
  if (!reply.trim()) return { wrong: 'it is empty' };
  const json = jsonOf(reply);
  if (json === undefined) return { wrong: `it is not JSON: ${JSON.stringify(excerpt(reply))}` };
  const result = output.safeParse(json);
  return result.success ? { value: result.data } : { wrong: z.prettifyError(result.error) };
}

/**
 * Answer `text` with a value of `output`: the agent's reply to it, parsed;
 * one that is not is answered with what is wrong, once, for another reply.
 */
export async function answerWith<O extends z.ZodObject>(
  text: string,
  output: O,
  prompt: (text: string) => Promise<string>,
): Promise<z.infer<O>> {
  let answer = parsed(output, await prompt(text));
  if ('wrong' in answer) {
    const again = `Your reply is not the answer asked for: ${answer.wrong}\nReply again, with the JSON only.`;
    answer = parsed(output, await prompt(again));
  }
  if ('wrong' in answer)
    throw new restate.TerminalError(`The agent's reply is not the answer asked for: ${answer.wrong}`);
  return answer.value;
}

/**
 * `prompt` (a framework's, of `options`), answering with a value of
 * `options.output` if it has one, or else with the agent's text.
 */
export const withOutput = <T extends { system?: string; output?: z.ZodObject }>(
  text: string,
  options: T,
  prompt: (text: string, options: T) => Promise<string>,
): Promise<Answer<T['output']>> => {
  const { output } = options;
  if (!output) return prompt(text, options) as Promise<Answer<T['output']>>;
  const asked = { ...options, system: [options.system, format(output)].filter(Boolean).join('\n\n') };
  return answerWith(text + reminder, output, (t) => prompt(t, asked)) as Promise<Answer<T['output']>>;
};
