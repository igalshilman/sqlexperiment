/**
 * 11 · A tool
 *
 * A tool is a function the model may call: a name and a description for the
 * model, its input as a zod schema, and `run`, which answers with text or
 * JSON. The agent calls it, hands the result to the model, and goes on. A
 * `TerminalError` is an answer too, for the model to correct itself; any
 * other error is a failure, which Restate retries.
 *
 *   node src/11_tool.ts   # and register it (see 01)
 *   curl localhost:8080/timekeeper/alice/chat -d 'What time is it in Lisbon and in Tokyo?'
 */
import { TerminalError } from 'restate-sessions';
import { serve, tool } from 'restate-agents';
import * as pi from 'restate-agents/pi';
import { z } from 'zod';

const clock = tool({
  name: 'time_in',
  description: 'The current time in a time zone, such as Europe/Lisbon.',
  input: z.object({ timeZone: z.string() }),
  run: ({ timeZone }) => {
    try {
      return new Date().toLocaleTimeString('en-GB', { timeZone });
    } catch {
      throw new TerminalError(`Unknown time zone: ${timeZone}`);
    }
  },
});

const timekeeper = pi.agent({ name: 'timekeeper', tools: [clock] });

await serve(timekeeper);
