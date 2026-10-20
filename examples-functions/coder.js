// A coding agent: Claude, with a shell and the files of a workspace as its
// tools, working on a task until it is done. The object's key names the
// workspace: a directory of the `workspace` service (under FUNCTIONS_WORKSPACES),
// so the next task finds the last one's files; each task's conversation is
// kept, in its database.
//
// The agent loop is plain JavaScript, and runs for as long as the task
// takes. The conversation is in the scope's database, and the loop in the
// heap, saved at each wait, on Claude or on a tool: a crash resumes the task
// where it was, and a model call or a command already answered is not made
// again.
//
//   ANTHROPIC_API_KEY=... target/release/functions
//   curl localhost:8080/coder/primes/work --json \
//     '{"task": "Write primes.py, printing the primes below 100, with a test, and run both"}'
//   curl localhost:8080/coder/primes/history
//   ls workspaces/primes
const MODEL = 'anthropic/claude-opus-5-5';
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

// The tasks so far: each one's conversation, from the task to the answer.
export async function history(_, ctx) {
  return ctx.llm.conversations().map((name) => {
    const messages = ctx.llm.conversation(name).history();
    const last = messages[messages.length - 1];
    const done = last.role === 'assistant' && !last.content.some((block) => block.type === 'tool_use');
    const steps = messages.filter((message) => message.role === 'assistant').length;
    return { task: messages[0].content, answer: done ? text(last.content) : null, steps };
  });
}

export async function work({ task }, ctx) {
  const tools = workspace(ctx);
  // The task's own conversation, kept in the scope's database: each step
  // sends only what it adds, and the host sends Claude the whole
  // conversation. It stays after the task, for `history`.
  const chat = ctx.llm.conversation(`task-${ctx.llm.conversations().length + 1}`, {
    model: MODEL,
    max_tokens: 32000,
    system: SYSTEM,
    tools: TOOLS,
    options: { output_config: { effort: 'medium' } },
  });
  let message = { role: 'user', content: task };
  for (let step = 1; step <= MAX_STEPS; step++) {
    const response = await chat.send({ messages: [message] });
    if (response.stop_reason === 'refusal') throw new Error('Claude declined the task');
    if (response.stop_reason !== 'tool_use') return { answer: text(response.content), steps: step };
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
    message = { role: 'user', content: results };
  }
  throw new Error(`The task took more than ${MAX_STEPS} steps`);
}

// A message's text, without its tool calls.
function text(content) {
  return content
    .filter((block) => block.type === 'text')
    .map((block) => block.text)
    .join('\n');
}
