# A tool gateway: MCP, approvals and limits, on Restate

An agent's tools behind one gateway. The tools are plain Restate services
with the journal. The gateway lists them, applies each tenant's policy, holds
the dangerous ones for a person's approval, limits how many run at once per
tenant and per user, and audits every call. A pi agent reaches it by RPC; any MCP client
(Claude Code, the MCP Inspector) reaches it over HTTP.

```
 pi agent (Assistant/acme.alice)        MCP client (Claude Code, …)
 pi-durable on ctx.db (SQLite)          Authorization: Bearer acme.alice
        │ RPC, idempotency key =               │ POST /mcp (mcp.ts): JSON-RPC
        │ request id : tool call id            │ → ingress, idempotency key =
        ▼                                      ▼   session : JSON-RPC id
 ┌─────────────────────────────────────────────────────────────────────┐
 │ Gateway (service, journaled)                                         │
 │  tools  the handlers with `mcp.tool` metadata, from the admin API,   │
 │         that the tenant's policy allows                              │
 │  call   policy → approval (awakeable; the manager is paged)          │
 │         → ctx.genericCall({ scope: tenant, limitKey: tool/user })    │
 │         → a record to Tenant/<tenant>                                │
 └─────────────────────────────────────────────────────────────────────┘
        │ in scope `acme`, limit key `lookup_order/alice`:
        │ rule `*/lookup_order` → 2 per tenant, `*/lookup_order/*` → 1 per user;
        │ the rest wait in the user's queue, and the users' queues take turns
        ▼
 Orders.lookup   Payments.refund   Mail.emailCustomer    (tools.ts: services, journaled)
        │ ctx.run, as the tenant (ctx.request().scope), for the user (a header)
        ▼
 the outside world (upstream.ts): an order system that serves 2 at once per
 API key and 429s the rest; a payment provider; a mail provider; a pager

 Tenant/acme (virtual object, SQL): the audit log and the approvals (tenant.ts)
```

## Each part uses the model that fits it

- **The agent's state is SQL.** pi-durable makes hundreds of state requests a
  prompt, so its session lives in the object's SQLite
  ([`../pi/with-pi.ts`](../pi/with-pi.ts)), and nothing is journaled.
- **The tools use the journal**: `ctx.run` around each request to the outside
  world, an awakeable for approval, a durable timeout. They know nothing about
  SQL, agents or MCP. A tool is a handler with metadata:

  ```ts
  refund: restate.handlers.handler(
    {
      description: 'Refund an amount of an order to its customer. A manager approves each refund.',
      metadata: { 'mcp.tool': 'refund', 'mcp.approval': 'required' },
      input: restate.serde.schema(Refund), // zod: its JSON schema is the tool's input schema
    },
    async (ctx, { order, amount, reason }) => …,
  ),
  ```

- **The audit log is SQL**: one table per tenant (`Tenant/<tenant>`), to query.
- **Idempotency keys connect them.** The agent has no journal: after a crash
  it reopens pi-durable from its database and runs the unfinished tool call
  again with the same call id. That makes the same idempotency key, so it
  reaches the gateway call the first attempt made. From there on the journal
  makes each step once.

## Scopes and limits

The gateway calls each tool in the tenant's **scope**, with a two-level
**limit key**: the tool, then the user it runs for.

```ts
ctx.genericCall({ service, method, parameter, scope: req.tenant, limitKey: `${tool.name}/${req.user}`, … });
```

The user comes from `<tenant>.<user>`: the agent's key (`Assistant/acme.alice`)
or the MCP bearer token (`acme.alice`; a bare `acme` is user `anonymous`).

A rule says how many calls run at once, at the scope (`acme`), the tool
(`acme/lookup_order`) or the user (`acme/lookup_order/alice`), with `*` for
any. The most specific rule wins, and a call holds a slot at every level.
For the order system's limit of two per API key, that is two per tenant:

```sh
restate rules set '*/lookup_order' --concurrency 2
# or: curl -X PUT localhost:9070/limits/rules --json '[{"pattern": "*/lookup_order", "limits": {"concurrency": 2}}]'
```

Calls over the limit wait in a queue; none fails. Each scope has its own
counter, so one tenant's burst never queues another's calls. Within a tenant
each user's calls have **a queue of their own** (a vqueue per limit key), and
the scheduler takes turns between them. So without any per-user rule, one
user's burst does not hold up another user's calls: they interleave. A rule on
the second level **caps each user**, so no user takes the whole of the tenant's
limit:

```sh
restate rules set '*/lookup_order/*' --concurrency 1          # each user: one at a time
restate rules set 'acme/lookup_order/alice' --concurrency 2   # but alice at acme: two
```

The cost of a cap: a user alone runs at the cap, even when the tenant has
slots free. A rule applies as soon as it is set, with no redeploy. To watch
it:

```sql
SELECT scope, l1, l2, usage, concurrency_limit, num_waiters FROM sys_user_limits;
SELECT scope, limit_key, num_inbox, num_running FROM sys_vqueue_meta;
```

Restate's rules limit **concurrency**, not calls per minute. Per-scope rate
limits are planned, not released. On restate-server 1.7, scopes and rules
need `RESTATE_EXPERIMENTAL_ENABLE_VQUEUES=true` and
`RESTATE_EXPERIMENTAL_ENABLE_PROTOCOL_V7=true`, on a new cluster. From 1.8
they are on by default.

## The demo

```sh
make examples-js                          # from the repository root
make gateway-demo                         # or: RESTATE_SERVER=/path/to/restate-server npm run demo:gateway
```

[`demo.ts`](demo.ts) runs a Restate server, the fake outside world, and the
tools, gateway and agents, each in its own process. The agents crash at
random state requests (`AGENT_ABORT_PER_MILLE`, default 2) and are restarted.
It narrates six acts and asserts each outcome:

1. **A burst, without a limit.** alice (acme) asks about 12 orders. The model
   calls `lookup_order` 12 times at once. The order system 429s some 20 to
   30 of the requests, and the tools' `ctx.run` retries them with backoff, so the
   answer takes ~15–20 s.
2. **The same burst, with the rule** set while everything runs. alice asks
   about 16 orders, and dan (globex) about 3 at the same time. The live view
   shows acme's calls running 2 at a time with the rest queued. The order
   system never sees more than 2 at once and refuses none, and globex's
   lookups do not wait in acme's queue.
3. **Users within a tenant.** alice asks about 16 orders; while they queue,
   erin (acme too) looks up 2 from an MCP client (token `acme.erin`). Each
   user's calls queue apart, and the queues take turns: the live view shows
   `acme 2/2 (alice 1 +7 queued, erin 1 +1 queued)`, and erin's lookups run
   while alice's still wait. Then the rule `*/lookup_order/*` at 1: alice asks
   about 6 and erin looks up 2, and the order system sees at most one of each
   user's requests at once (`alice 1/1 +3 queued, erin 1/1`).
4. **Human in the loop.** bob asks for a refund. The gateway holds it on an
   awakeable and pages the manager; its invocation is _suspended_. The demo
   kills the agents' process with `kill -9`, then the manager approves
   (`Tenant/acme/decide`). The answer arrives, and the payment provider made
   exactly one refund.
5. **MCP.** A client with dan's token at globex (`globex.dan`) lists its tools (no `refund`: the
   policy), calls `lookup_order`, is refused `refund` (`isError`), and sends one
   `email_customer` twice with the same JSON-RPC id. It gets the same result
   twice, and one mail is sent.
6. **The audit log.** `Tenant/<tenant>/report` queries each tenant's table,
   per tool and per user: one record per call however often the agents
   crashed (54 lookups for acme),
   and a `tool invocations` count from `sys_invocation` of exactly one per
   tool call.

It passed four runs in a row, with 5 to 16 restarts of the agents' process each.

## By hand

```sh
cd examples
node gateway/upstream.ts &                # the fake outside world, on 9300 (it prints the pages)
node serve.ts gateway &                   # tools, gateway, Tenant and Assistant, on 9080
node gateway/mcp.ts &                     # the MCP endpoint, on 9400
RESTATE_EXPERIMENTAL_ENABLE_VQUEUES=true RESTATE_EXPERIMENTAL_ENABLE_PROTOCOL_V7=true restate-server &
restate deployments register http://localhost:9080
restate rules set '*/lookup_order' --concurrency 2
restate rules set '*/lookup_order/*' --concurrency 1

curl localhost:8080/Assistant/acme.alice/prompt --json '{"id": "p1", "text": "where are orders 101 to 112?"}'
curl localhost:8080/Assistant/acme.bob/prompt --json '{"id": "p2", "text": "refund 30 of order 103 because it broke"}' &
curl localhost:8080/Tenant/acme/pending --json '{}'
curl localhost:8080/Tenant/acme/decide --json '{"awakeable": "sign_…", "approved": true, "by": "carol"}'
curl localhost:8080/Tenant/acme/report --json '{}'

claude mcp add --transport http restate-tools http://127.0.0.1:9400/mcp --header "Authorization: Bearer acme.alice"
```

`PI_MODEL=anthropic/claude-sonnet-4-5` (with `ANTHROPIC_API_KEY`) replaces the
scripted model with a real one. The tools are the same; they come from the
gateway.

## Files

- [`tools.ts`](tools.ts): the tools, as plain journaled services.
- [`gateway.ts`](gateway.ts): the registry, policy, approval, scoped calls, and
  audit records.
- [`tenant.ts`](tenant.ts): the Tenant object: the audit log and the approvals,
  in SQL.
- [`agent.ts`](agent.ts): the pi agent, with the gateway's tools, and the
  scripted model (the concierge).
- [`mcp.ts`](mcp.ts): the MCP endpoint (Streamable HTTP, JSON answers,
  stateless).
- [`upstream.ts`](upstream.ts): the fake outside world.
- [`demo.ts`](demo.ts): the demo, against a Restate server.
- [`gateway.test.ts`](gateway.test.ts): the registry, the MCP endpoint and the
  concierge, without a server (`npm run test:gateway`).
