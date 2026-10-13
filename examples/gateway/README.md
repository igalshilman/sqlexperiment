# A tool gateway on Restate

A pi agent and MCP clients call tools through one gateway, which checks
policy, holds dangerous calls for approval, limits calls per tenant and per
user, and audits each call.

```
  ┌──────────────────────────┐        ┌──────────────────────────┐
  │ Assistant  (agent.ts)    │        │ MCP client (Claude Code) │
  │ pi agent, state in SQL   │        │ → mcp.ts                 │
  └────────────┬─────────────┘        └────────────┬─────────────┘
               │ RPC                               │ HTTP
               ▼                                   ▼
  ┌─────────────────────────────────────────────────────────────┐
  │ Gateway  (gateway.ts, journaled)                            │
  │ registry · policy · approval · scope + limit key · audit    │──▶ Tenant (tenant.ts, SQL)
  └────────────────────────────┬────────────────────────────────┘    audit log, held calls
                               │ scope: tenant, limit key: tool/user
                               ▼
  ┌─────────────────────────────────────────────────────────────┐
  │ Tools  (tools.ts, journaled): Orders.lookup, Payments.refund│
  └────────────────────────────┬────────────────────────────────┘
                               ▼
              fake order system and payments (upstream.ts)
```

Run it:

```sh
make examples-js && make gateway-demo
```

[`demo.ts`](demo.ts) plays six acts against a Restate server, with the
agents crashing at random, and checks each one: a burst without limits, a
limit per tenant, a limit per user, an approval, an MCP client, and the
audit log.

Limits are Restate rules on the tool and user, for example:

```sh
restate rules set '*/lookup_order' --concurrency 2     # per tenant
restate rules set '*/lookup_order/*' --concurrency 1   # per user
```

On restate-server 1.7, set `RESTATE_EXPERIMENTAL_ENABLE_VQUEUES=true` and
`RESTATE_EXPERIMENTAL_ENABLE_PROTOCOL_V7=true`.
