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

[`demo.ts`](demo.ts) plays six acts against a Restate server, with the
agents crashing at random, and checks each one: a burst without limits, a
limit per tenant, a limit per user, an approval, an MCP client, and the
invocations the server recorded.

Limits are Restate rules on the tool and user, for example:

```sh
restate rules set '*/lookup_order' --concurrency 2     # per tenant
restate rules set '*/lookup_order/*' --concurrency 1   # per user
```

On restate-server 1.7, set `RESTATE_EXPERIMENTAL_ENABLE_VQUEUES=true` and
`RESTATE_EXPERIMENTAL_ENABLE_PROTOCOL_V7=true`.
