# ctx.llm: a model gateway on the host

Programs ask models through the host's gateway, [`Llm`](../src/llm.rs). They
use one format for every provider, and can keep a conversation in their scope,
sending only what is new.

```js
const chat = ctx.llm.conversation('task', {
  model: 'anthropic/claude-opus-5-5', // or openai/gpt-5, xai/grok-4
  system: 'You are a coding agent.',
  tools: TOOLS,
});
let response = await chat.send({ messages: [{ role: 'user', content: task }] });
// ... run the tools it asked for, then:
response = await chat.send({ messages: [{ role: 'user', content: results }] });
```

```python
chat = ctx.llm.conversation("task", model="openai/gpt-5", tools=TOOLS)
response = await chat.send({"role": "user", "content": task})
```

A whole request, with no conversation: `ctx.llm.complete({model, messages,
...})`.

## One format

The format is the Anthropic Messages API's, as the gateway's own:

- **Messages:** `user` and `assistant` messages.
- **Content:** text, `tool_use` and `tool_result` blocks.
- **Response:** `{content, stop_reason, usage}`.
- **Models:** named `<provider>/<model>`.
- **Provider options:** anything provider-specific goes in `options`, merged
  into the provider's request as it is (such as
  `{output_config: {effort: 'low'}}`).

The host translates for each provider:

| Provider                                   | On the wire                                                                                                                             |
| ------------------------------------------ | --------------------------------------------------------------------------------------------------------------------------------------- |
| `anthropic`                                | The Messages API, nearly as is, with a cache breakpoint at the conversation's end                                                       |
| `openai`, `xai`, any OpenAI-compatible one | Chat completions: the system prompt as a message, tool calls as `tool_calls`, tool results as `tool` messages; back again on the way in |

The host holds the keys (ANTHROPIC_API_KEY, OPENAI_API_KEY, XAI_API_KEY): a
program never has one. It retries rate limits, overloads and network errors,
with backoff, up to 8 times.

## Where a conversation lives

A conversation is a table, `llm_messages`, in the scope's SQLite database,
next to the program's own tables and its KV:

1. **`send`.** The program adds its new messages to the table, in the turn,
   and issues the call with the conversation's name and its defaults (model,
   system prompt, tools).
2. **Commit.** The turn commits: the messages are in the database, and
   "waiting on `llm` #7" in the commit.
3. **The call.** The host reads the whole conversation from the database,
   builds the provider's request, and sends it.
4. **The answer.** It is delivered to the program, which adds it to the
   table in that turn, and continues.

A crash before 4 sends the same request again, read from the same committed
messages. The question is in the table once, and so is the answer.

So the conversation is in neither the program's memory nor Restate's
journal. The program holds its loop and the latest response. The coding
session of DESIGN.md §10.1 (200 steps, 636 KiB of conversation at the end)
compares like this:

| Where the conversation is                                           | Program memory | Stored over 200 steps | Biggest writes                       |
| ------------------------------------------------------------------- | -------------: | --------------------: | ------------------------------------ |
| The program's heap, sent by a host binding (before `fetch`)         |        3.6 MiB |               2.6 MiB | New heap bases, to 235 KB            |
| The program's heap, sent with `fetch(url, {body: fetch.json(...)})` |        3.6 MiB |               3.2 MiB | New heap bases, to 244 KB            |
| The scope's database (`ctx.llm.conversation`)                       |        1.5 MiB |               2.5 MiB | Database checkpoints, 31–41 KB, flat |

Reading the conversation back for a call takes 0.5 ms at the start and 3.6 ms
at 636 KiB. Turns take 1–2 ms, a little more than with the conversation in
the heap, for the rows they write.

One conversation has one call at a time: a second `send` before the first is
answered fails.

## How long a conversation lasts

A conversation is the scope's, not the run's: it begins with its first
`send`, and stays, through the end of the run, crashes and new versions of
the program, until the program clears it (`clear()`). There is no expiry.
`ctx.llm.conversations()` lists the scope's conversations, in the order they
began.

The coding agent keeps one per task (`task-1`, `task-2`, ...): its
`history` handler reads them back, each from its task to its answer.

## What this is not, yet

- **Streaming.** An answer arrives whole. The gateway is where streaming
  would go: it could publish the tokens to a UI while the program waits for
  the whole answer.
- **Other providers' own APIs.** OpenAI's Responses API, Gemini and Bedrock
  have no adapter yet. Images and documents are not translated.
- **Sharing.** A conversation belongs to its scope. Handing one to another
  agent means sending it as messages.
- **Harnesses.** An embedded framework sends its whole message list each
  time: it would use `complete`, and keep the conversation itself.
- **Usage and limits.** Usage is in each response, but nothing totals it
  yet, nor caps a scope's spend.
