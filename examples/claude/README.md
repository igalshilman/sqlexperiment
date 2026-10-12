# A Claude agent in a Restate virtual object

The Anthropic SDK's tool runner (`client.beta.messages.toolRunner` in
[`@anthropic-ai/sdk`](https://github.com/anthropics/anthropic-sdk-typescript)) is a light agent
harness: the tool-calling loop, in the process, with nothing stored. It keeps
the conversation in its params, so an agent built on it is durable when the
conversation is: here it is in a Restate virtual object's SQL database
([SQL in Restate virtual objects](../node/README.md)), one object per
conversation.

It is lighter than the Claude Agent SDK (`@anthropic-ai/claude-agent-sdk`),
which runs Claude Code as a subprocess and mirrors its transcripts to a store
after the fact (`sessionStore`), so a tool can run before its call is stored.
Claude Code's own tools (bash, read, write, edit, glob, grep) come to the
runner as `betaAgentToolset20260401` from `@anthropic-ai/sdk/tools/agent-toolset/node`;
this example has only its bank tools.

- [`conversation.ts`](conversation.ts): `Conversation`, the messages over
  the object's database (the `db` of `ctx.sqlSession`), in order, each with
  the id of the prompt it belongs to, and the answer to each prompt. Each
  `append` is one SQL transaction.
- [`agent.ts`](agent.ts): a ClaudeAgent object per conversation. Its
  handler, `prompt(ctx, { id, text })`, starts a runner with the stored
  messages, in one `ctx.sqlSession`, and stores each step:

  ```js
  for await (const message of runner) {
    const reply = { role: 'assistant', content: message.content };
    if (!message.content.some((c) => c.type === 'tool_use')) {
      await conversation.append(id, [reply], textOf(message.content));
      return;
    }
    // The reply first, then its tools.
    await conversation.append(id, [reply]);
    const results = await runner.generateToolResponse();
    check();
    await conversation.append(id, [results]);
  }
  ```

  Its tools move money in the [ledger](../node/ledger.ts).

- [`teller.ts`](teller.ts): a scripted model that needs no API key: an
  Anthropic client whose `fetch` answers the Messages API itself, so the agent
  and its tests run offline. `CLAUDE_MODEL=claude-sonnet-5-5` (and
  `ANTHROPIC_API_KEY`) uses Claude instead.
- [`agent.test.ts`](agent.test.ts): the handler with a fake `ctx`, through
  crashes.

## Through crashes

The handler is not replayed: a retried attempt runs it again from the start,
with the stored messages.

- **Each reply is stored before its tools run, and their results before the
  next request.** The runner runs a reply's tools after the loop body, and
  `generateToolResponse()` in the body runs them there instead (the runner
  keeps their results and does not run them again). So a crash during the
  tools resumes them with the same tool call ids, without asking the model
  again: an attempt that finds a stored reply with no results runs its tools
  first.
- **A tool is idempotent by its tool call id** (`context.toolUse.id`): it uses
  it in the transfer id, which the ledger dedupes on, so a tool run again moves
  the money once. Its call to the ledger has an idempotency key made from the
  request id (`ctx.request().id`, the same on every attempt) and the tool call
  id, so a retried attempt reaches the call the first one made.
- **A tool's error fails the attempt**, and Restate retries it. By default the
  runner would hand the error to the model as the tool's result, and the model
  would answer with it.
- **A prompt is answered once per id**: its answer is stored with the model's
  last reply, and a prompt asked again returns it. A prompt that a crash
  interrupted is finished before the next prompt starts.

[`agent.test.ts`](agent.test.ts) checks each of these, with a crash that
stops an attempt in a tool call and abandons it, keeping what it committed;
storing the reply with its results, after the tools, or handing a tool's
error to the model, fails it. The [crash test](../README.md#validation) runs
four conversations of eight transfers and an audit each, beside the pi and
LangGraph agents', against a real server: every prompt moves money exactly
once.

## Run

```sh
make examples-js   # from the repository root
cd examples && node claude/agent.ts   # the agent and the ledger, on port 9080
docker run --rm -p 8080:8080 -p 9070:9070 --add-host=host.docker.internal:host-gateway ghcr.io/restatedev/restate:1.7.13   # Restate (see the repository's README, Requirements), in another terminal
restate deployments register http://host.docker.internal:9080   # as the server's container sees this host
curl localhost:8080/Ledger/bank/init --json '[8, 1000]'
curl localhost:8080/ClaudeAgent/alice/prompt --json '{"id": "1", "text": "Please move 5 from 1 to 2"}'
npm run test:claude   # the handler's tests
```
