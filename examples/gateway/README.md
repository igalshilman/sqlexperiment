# A tool gateway on Restate

A pi agent and MCP clients call tools through one gateway per tenant, which
checks policy, holds dangerous calls for approval, and limits calls per
tenant and per user.

```
  ┌──────────────────────────┐        ┌──────────────────────────┐
  │ Assistant  (agent.ts)    │        │ MCP client (Claude Code) │
  │ pi agent, state in SQL   │        │ → mcp.ts                 │
  └────────────┬─────────────┘        └────────────┬─────────────┘
               │ RPC                               │ HTTP
               ▼                                   ▼
  ┌─────────────────────────────────────────────────────────────┐
  │ Gateway/<tenant>  (gateway.ts, journaled)                   │
  │ registry · policy · approval · scope + limit key            │
  └────────────────────────────┬────────────────────────────────┘
                               │ scope: tenant, limit key: tool/user
                               ▼
  ┌─────────────────────────────────────────────────────────────┐
  │ Tools  (tools.ts, journaled): Orders.lookup, Payments.refund│
  └────────────────────────────┬────────────────────────────────┘
                               ▼
              fake order system and payments (upstream.ts)
```

Where each part runs: every call goes through the Restate server, which
pushes each invocation to the endpoint that serves it. No part calls
another directly, and none keeps state of its own.

```
  ┌─ VM ──────────────────────┐            ┌─ anywhere ────────────────┐
  │ agent.ts                  │            │ MCP client (Claude Code)  │
  │  Assistant/acme.alice     │            │        │                  │
  │  pi-durable, SQLite pages │            │        ▼                  │
  │  cached; none kept here   │            │ mcp.ts (MCP → ingress)    │
  └─────────────▲─────────────┘            └────────┬──────────────────┘
                │ invocations (HTTP/2)              │ HTTP
                │ and the pages it reads, writes    │
  ┌─────────────┴───────────────────────────────────▼──────────────────────┐
  │ Restate server                                                         │
  │  ingress :8080 · admin :9070 (services and their metadata, rules, SQL) │
  │  journals of the gateway and tools                                     │
  │  state of the objects: the agent's SQLite pages, the held calls       │
  │  scopes and vqueues: calls over a limit wait here, per tenant and user │
  │  awakeables: a call held for approval is suspended here                │
  └─────────────┬──────────────────────────────────────────┬───────────────┘
                │ invocations                             │ invocations
  ┌─ host ──────▼──────────────┐            ┌─ elsewhere ──▼──────────────┐
  │ gateway.ts                 │            │ tools.ts                    │
  │  Gateway (reads admin API) │            │  Orders, Payments           │
  │                            │            │  (a container, a function)  │
  └────────────────────────────┘            └──────────────┬──────────────┘
                                                           │ HTTP, ctx.run
                                                           ▼
                                             order system, payment provider
```

Kill the VM in the middle of a prompt and the agent resumes on any other:
its session is in Restate, and its unfinished tool call reaches the same
gateway call by its idempotency key.

Run it:

```sh
make examples-js && make gateway-demo
```

[`demo.ts`](demo.ts) plays five acts against a Restate server, with the
agents crashing at random, and checks each one:

1. Tools are just handlers: each tenant's tools, found by their metadata.
2. Crash anytime: a burst of lookups through crashes and 429s, each order
   served once.
3. Limits without code: a rule per tenant, then per user.
4. Human in the loop: a refund held for approval while the agents are killed.
5. Any client: an MCP client, with the same policy and idempotency.

Limits are Restate rules on the tool and user, for example:

```sh
restate rules set '*/lookup_order' --concurrency 2     # per tenant
restate rules set '*/lookup_order/*' --concurrency 1   # per user
```

In Docker, one image runs every part: [`Dockerfile`](Dockerfile), published
as `ghcr.io/igalshilman/tool-gateway`. Its default command serves the
Restate services on 9080; `node gateway/upstream.ts` and
`node gateway/mcp.ts` run the others. `RESTATE_ADMIN_URL`, `UPSTREAM_URL`,
`RESTATE_INGRESS_URL` and `HOST` say where the parts are.

On restate-server 1.7, set `RESTATE_EXPERIMENTAL_ENABLE_VQUEUES=true` and
`RESTATE_EXPERIMENTAL_ENABLE_PROTOCOL_V7=true`.
