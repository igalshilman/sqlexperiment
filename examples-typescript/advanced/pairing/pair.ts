/**
 * Talk with the pair from a terminal, about a project (its key):
 *
 *   node advanced/pairing/pair.ts fizzbuzz
 *
 * What you type goes to the pair; while it works on a message, a line steers
 * it, at its next tool call. Its messages and its tool calls show as it makes
 * them, and so do its waits for your approval. Commands:
 *
 *   /approve [what to change]   answer its wait for your approval
 *   /tasks                      the plan
 *   /stop                       stop the message it works on
 *
 * Ctrl-D leaves. The pair carries on without you, and running this again
 * shows the conversation so far, and follows it from there.
 */
import { createInterface } from 'node:readline/promises';
import * as clients from '@restatedev/restate-sdk-clients';
import { type Agent, agent, type Followed, follow, type Part } from 'restate-agents/client';
import type { Task } from './tools.ts';

/** The pair's handlers: an agent's, and its own `tasks`. */
interface Pair extends Agent {
  tasks(ctx: unknown): Promise<Task[]>;
}

/** What the terminal showed of the conversation: each part of a message once, and a tool's output once. */
class Terminal {
  readonly #shown = new Set<string>();
  // The first change is the conversation so far, your messages in it too; after it, you typed them.
  #history = true;

  show({ messages, waiting }: Followed) {
    for (const { id, role, parts } of messages) {
      if (role === 'user' && !this.#history) continue;
      parts.forEach((part, i) => this.#part(`${id}.${i}`, role, part));
    }
    for (const wait of waiting) {
      this.#once(wait.id, `⏸  ${wait.name}: ${JSON.stringify(wait.input)}  (/approve, or /approve <what to change>)`);
    }
    this.#history = false;
  }

  #part(key: string, role: string, part: Part) {
    if (part.type === 'text') return this.#once(key, `${role === 'user' ? 'you' : 'pair'}: ${part.text}`);
    this.#once(key, `  → ${part.name}(${clipped(JSON.stringify(part.input))})`);
    if (part.output !== undefined) this.#once(`${key}.output`, `  ← ${clipped(part.output)}`);
  }

  #once(key: string, text: string) {
    if (this.#shown.has(key)) return;
    this.#shown.add(key);
    console.log(text);
  }
}

/** `text` on one line, at most `limit` characters of it. */
function clipped(text: string, limit = 160) {
  const line = text.replaceAll('\n', ' ⏎ ');
  return line.length <= limit ? line : `${line.slice(0, limit - 1)}…`;
}

const project = process.argv[2];
if (!project) throw new Error('Usage: node advanced/pairing/pair.ts <project>');
const ingress = clients.connect({ url: 'http://localhost:8080' });
const conversation = ingress.objectClient(agent<Pair>('pair'), project);

/** A command, or a message: to the message the pair works on, if any, or else as one of its own. */
async function say(line: string) {
  const [command, ...words] = line.split(' ');
  const rest = words.join(' ');
  switch (command) {
    case '/approve': {
      const { resolved } = await conversation.signal({ name: 'approval', value: rest || 'approved' });
      if (!resolved) console.log('Nothing waits for your approval.');
      return;
    }
    case '/tasks':
      for (const { n, task, done } of await conversation.tasks()) console.log(`  [${done ? 'x' : ' '}] ${n}. ${task}`);
      return;
    case '/stop':
      await conversation.stop();
      return;
    default:
      await conversation.steer(line);
  }
}

const terminal = new Terminal();
const following = new AbortController();
const shown = (async () => {
  const changes = follow(ingress, agent('pair'), project, { signal: following.signal });
  for await (const change of changes) terminal.show(change);
})();

for await (const line of createInterface({ input: process.stdin })) {
  try {
    if (line.trim()) await say(line.trim());
  } catch (error) {
    console.log(`Restate answered ${String(error)}`);
  }
}
following.abort();
await shown;
