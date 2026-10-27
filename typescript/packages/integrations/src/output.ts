// An agent's answer as data, with `output` (every framework's option): a zod
// object schema. The agent hands its answer in with a tool, `submit_answer`,
// whose input is the schema, so the model's call is checked against it; the
// prompt then answers with that value, not the model's text.
//
//   const planner = claude.agent({ name: 'planner', output: Plan });   // chat answers with a Plan
//
// The value is what the tool was called with, kept in memory, and the prompt
// answers with it directly. A model that ends its turn without handing an
// answer in is asked once more, then the prompt fails (a terminal error); so
// is one whose turn a crash cut after it handed its answer in, as the retry
// resumes the conversation past the tool's call: it hands it in again.
import * as restate from 'restate-fns';
import type { z } from 'zod';
import { tool, type Tool } from './tools.ts';

/** What a prompt answers: its text, or with `output`, the value the agent handed in. */
export type Answer<O> = O extends z.ZodObject ? z.infer<O> : string;

/** The tool's name, which the reminder names too. */
const SUBMIT = 'submit_answer';

/** What the agent is asked when its turn ended without an answer (in this attempt). */
const REMIND = `Hand in your answer now: call \`${SUBMIT}\` with it.`;

/**
 * Answer `text` with a value of `output`: `prompt` (the framework's) asks
 * the agent with the tool that hands it in beside its own, and again, once,
 * if it did not.
 */
export async function answerWith<O extends z.ZodObject>(
  text: string,
  output: O,
  prompt: (text: string, tools: Tool[]) => Promise<string>,
): Promise<z.infer<O>> {
  let answer: unknown;
  const submit = tool({
    name: SUBMIT,
    description: 'Hand in your answer: what the one who asked gets. Hand it in again to change it.',
    input: output,
    run: async (value) => {
      answer = value;
      return 'Handed in.';
    },
  });
  await prompt(text, [submit]);
  if (answer === undefined) await prompt(REMIND, [submit]);
  if (answer === undefined) throw new restate.TerminalError(`The agent handed in no answer (with ${SUBMIT})`);
  return output.parse(answer);
}

/**
 * `prompt` (a framework's, of `options`), answering with a value of
 * `options.output` if it has one, or else with the agent's text.
 */
export const withOutput = <T extends { tools?: unknown[]; output?: z.ZodObject }>(
  text: string,
  options: T,
  prompt: (text: string, options: T) => Promise<string>,
): Promise<Answer<T['output']>> =>
  (options.output
    ? answerWith(text, options.output, (t, tools) =>
        prompt(t, { ...options, tools: [...(options.tools ?? []), ...tools] }),
      )
    : prompt(text, options)) as Promise<Answer<T['output']>>;
