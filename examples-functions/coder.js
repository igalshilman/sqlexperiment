// A coding agent: Claude, with a shell and the files of a workspace as its
// tools, working on a task until it is done. The object's key names the
// workspace: a directory of the `workspace` service (under FUNCTIONS_WORKSPACES),
// so the next task finds the last one's files; the tasks done so far are
// in its KV.
//
// The agent loop is plain JavaScript, and runs for as long as the task
// takes. At each wait, on Claude or on a tool, the conversation and the loop
// are saved with the rest of the heap: a crash resumes the task where it
// was, and a model call or a command already answered is not made again.
//
//   ANTHROPIC_API_KEY=... target/release/functions
//   curl localhost:8080/coder/primes/work --json \
//     '{"task": "Write primes.py, printing the primes below 100, with a test, and run both"}'
//   curl localhost:8080/coder/primes/history
//   ls workspaces/primes
const MODEL = 'claude-opus-5-5';
const MAX_STEPS = 200;

const SYSTEM = `You are a coding agent working in a workspace: a directory, with a shell.
Use the tools to do the task: look around first, make the changes, and run
what you wrote to check it. Paths are relative to the workspace. When the task
is done, answer with a short summary of what you did.`;

const TOOLS = [
  {
    name: 'bash',
    description:
      'Run a bash command in the workspace, and get its exit code and output (stdout, then stderr). ' +
      'Each command starts afresh in the workspace: no state is kept between commands.',
    input_schema: {
      type: 'object',
      properties: {
        command: { type: 'string' },
        timeout: { type: 'integer', description: 'Seconds before the command is killed (default 120, at most 600)' },
      },
      required: ['command'],
      additionalProperties: false,
    },
  },
  {
    name: 'read_file',
    description: 'The text of a file.',
    input_schema: {
      type: 'object',
      properties: { path: { type: 'string' } },
      required: ['path'],
      additionalProperties: false,
    },
  },
  {
    name: 'write_file',
    description: 'Create or overwrite a file, and the directories to it.',
    input_schema: {
      type: 'object',
      properties: { path: { type: 'string' }, content: { type: 'string' } },
      required: ['path', 'content'],
      additionalProperties: false,
    },
  },
  {
    name: 'edit_file',
    description:
      'Replace old_text with new_text in a file. old_text must occur exactly once: ' +
      'include enough of the lines around it to make it unique.',
    input_schema: {
      type: 'object',
      properties: { path: { type: 'string' }, old_text: { type: 'string' }, new_text: { type: 'string' } },
      required: ['path', 'old_text', 'new_text'],
      additionalProperties: false,
    },
  },
];

// The tools, as calls to the workspace.
function workspace(ctx) {
  const call = (handler, input) => ctx.rpc.call({ service: 'workspace', handler, key: ctx.key }, input);
  return {
    async bash(input) {
      const { exit, output } = await call('bash', input);
      return `exit ${exit}\n${output}`;
    },
    read_file: ({ path }) => call('read', { path }),
    write_file: ({ path, content }) => call('write', { path, content }),
    edit_file: ({ path, old_text, new_text }) => call('edit', { path, old: old_text, new: new_text }),
  };
}

export async function history(_, ctx) {
  return (await ctx.kv.get('history')) ?? [];
}

export async function work({ task }, ctx) {
  const tools = workspace(ctx);
  const messages = [{ role: 'user', content: task }];
  for (let step = 1; step <= MAX_STEPS; step++) {
    const response = await ctx.call('claude.messages', {
      model: MODEL,
      max_tokens: 32000,
      output_config: { effort: 'medium' },
      system: SYSTEM,
      tools: TOOLS,
      messages: cached(messages),
    });
    if (response.stop_reason === 'refusal') throw new Error('Claude declined the task');
    messages.push({ role: 'assistant', content: response.content });
    if (response.stop_reason !== 'tool_use') {
      const text = response.content.filter((block) => block.type === 'text');
      const answer = text.map((block) => block.text).join('\n');
      const history = (await ctx.kv.get('history')) ?? [];
      await ctx.kv.set('history', [...history, { task, answer, steps: step }]);
      return { answer, steps: step };
    }
    // Every tool the model asked for, in order: commands may depend on the
    // files written before them. The results go back in one message.
    const results = [];
    for (const { id, name, input } of response.content.filter((block) => block.type === 'tool_use')) {
      try {
        if (!(name in tools)) throw new Error(`No such tool: ${name}`);
        results.push({ type: 'tool_result', tool_use_id: id, content: await tools[name](input) });
      } catch (error) {
        results.push({ type: 'tool_result', tool_use_id: id, content: error.message, is_error: true });
      }
    }
    messages.push({ role: 'user', content: results });
  }
  throw new Error(`The task took more than ${MAX_STEPS} steps`);
}

// The conversation, with a cache breakpoint at its end: each request reads
// what came before from the cache, and pays only for what it adds.
function cached(messages) {
  const last = messages[messages.length - 1];
  const content = typeof last.content === 'string' ? [{ type: 'text', text: last.content }] : last.content;
  const marked = { ...content[content.length - 1], cache_control: { type: 'ephemeral' } };
  return [...messages.slice(0, -1), { ...last, content: [...content.slice(0, -1), marked] }];
}
