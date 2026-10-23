# A tool gateway on Restate

A pi agent calls tools through one gateway per tenant, which holds
dangerous calls for approval, and limits calls per tenant and per user. It is one endpoint, and everything else is Restate.

```
  ┌─────────────────────────────────────────────────────────────┐
  │ Assistant/<tenant>.<user>  (agent.ts)                       │
  │ pi agent, its session in the object's SQL database          │
  └───────┬──────────────────────────────────┬──────────────────┘
          │                                  │ investigate:
          │                                  │ RPC: Case.report, per order
          │                                  ▼
          │                ┌────────────────────────────────────┐
          │                │ Case/<tenant>.<order>  (agent.ts)  │
          │                │ pi agent per order, own database   │
          │                └─────────────────┬──────────────────┘
          │ RPC: Gateway.tools, Gateway.call │
          ▼                                  ▼
  ┌─────────────────────────────────────────────────────────────┐
  │ Gateway/<tenant>  (gateway.ts, journaled)                   │
  │ prebuilt tools · approval · scope + limit key               │
  └────────────────────────────┬────────────────────────────────┘
                               │ scope: tenant, limit key: tool/user
                               ▼
  ┌─────────────────────────────────────────────────────────────┐
  │ Tools  (tools.ts, journaled): Orders.lookup, Payments.refund│
  │ Support.ticket, over fake outside systems, in process       │
  └─────────────────────────────────────────────────────────────┘
```

Restate holds the rest:

- the agent's session, as the object's SQLite pages in its state;
- the held calls, each suspended on an awakeable until a manager decides;
- the limits: calls over a rule wait in Restate's queues, per tenant and per
  user.

Run the endpoint, from [`examples/`](..) (after `make js` at the repository
root), register it, and play the demo against it:

```sh
MODEL=openai/gpt-6-luna node advanced/gateway/main.ts
npx @restatedev/restate deployments register http://localhost:9080
advanced/gateway/demo.sh
```

[`demo.sh`](demo.sh) plays four acts through Restate's ingress and admin API:

1. Tools are just handlers: the prebuilt tools, and a prompt that uses them.
2. Limits without code: a rule per tenant and one per user.
3. Human in the loop: a refund held until a manager approves it.
4. Sub-agents: the `investigate` tool hands each order to its case agent,
   `Case/<tenant>.<order>`: a pi agent per order, with a database of its own.
   Two orders are two objects, which run at once; another user's report on
   the same order reaches the same object, which remembers the first; and
   another tenant's order of the same number is another object, empty.

`MODEL` is the agents' model, such as `openai/gpt-6-luna` with
`OPENAI_API_KEY` (by default `anthropic/claude-sonnet-5-5`, with
`ANTHROPIC_API_KEY`).
On restate-server 1.7, set `RESTATE_EXPERIMENTAL_ENABLE_VQUEUES=true` and
`RESTATE_EXPERIMENTAL_ENABLE_PROTOCOL_V7=true`.

Or as two deployments: `node advanced/gateway/main.ts tools` (the tools and the
Gateway: stateless, so anywhere, such as your laptop) and
`node advanced/gateway/main.ts agents` (the Assistant, keyed per user, the Case,
keyed per order: in a sandbox). Register both with the same Restate server.

With `WORKSPACE` set, each agent object gets a directory of its own,
`$WORKSPACE/<service>/<key>`, and pi's coding tools over it: `read`, `write`,
`edit` and `bash`, on the local file system. The files are on the
disk, not in Restate: the conversation outlives a lost disk, its files do
not. `bash` runs any command, so run it in a sandbox.

The image's model is `openai/gpt-6-luna`: give it `OPENAI_API_KEY`.

In Docker: [`Dockerfile`](Dockerfile), published as
`ghcr.io/igalshilman/tool-gateway`, serves the endpoint on 9080.

[`tests/gateway.test.ts`](../../../typescript/tests/gateway.test.ts) tests its parts
without a Restate server (`make test-ts T=gateway`).
