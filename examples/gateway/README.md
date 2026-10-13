# A tool gateway: MCP, approvals and limits, on Restate

An agent's tools behind one gateway. The tools are plain Restate services
with the journal. The gateway lists them, applies each tenant's policy, holds
the dangerous ones for a manager's approval, limits how many run at once per
tenant and per user, and audits every call. A pi agent reaches it by RPC, and
any MCP client (Claude Code, say) over HTTP.

```
 pi agent (Assistant/acme.alice)          MCP client, token acme.erin
 its session in its object's SQLite       POST /mcp (mcp.ts) → ingress
        │ RPC, idempotency key =                 │ idempotency key =
        │ request id : tool call id              │ session : JSON-RPC id
        ▼                                        ▼
 Gateway (service, journaled): tools from the admin API → policy
   → approval (an awakeable) → ctx.genericCall({ scope: tenant, limitKey: tool/user })
   → a record in Tenant/<tenant> (a virtual object, SQL: audit log and held calls)
        │  rules: */lookup_order → 2 per tenant, */lookup_order/* → 1 per user
        ▼
 Orders.lookup, Payments.refund (tools.ts: services, journaled; ctx.run outside)
        ▼
 the outside world (upstream.ts): an order system that serves 2 at once per
 tenant and 429s the rest; a payment provider
```

- **The agent's state is SQL** ([`../pi/with-pi.ts`](../pi/with-pi.ts)): it
  has no journal. After a crash it reopens its session and runs the
  unfinished tool call again, with the same call id. That makes the same
  idempotency key, so it reaches the gateway call the first attempt made.
- **The gateway and the tools use the journal**, so each approval, tool call
  and audit record happens once. A tool is a handler with metadata:

  ```ts
  refund: restate.handlers.handler(
    {
      description: 'Refund an amount of an order. A manager approves each refund.',
      metadata: { 'mcp.tool': 'refund', 'mcp.approval': 'required' },
      input: restate.serde.schema(Refund), // zod: the tool's input schema
    },
    async (ctx, { order, amount }) => …,
  ),
  ```

## Scopes and limits

The gateway calls each tool in the tenant's **scope**, with `<tool>/<user>`
as **limit key**; the user comes from `<tenant>.<user>`, the agent's key or
the MCP token. A rule limits the calls at once at any level, the most
specific one winning:

```sh
restate rules set '*/lookup_order' --concurrency 2            # each tenant
restate rules set '*/lookup_order/*' --concurrency 1          # each user
restate rules set 'acme/lookup_order/alice' --concurrency 2   # but alice at acme
```

Calls over a limit wait in a queue; none fails. Each tenant has its own
counter, so one tenant's burst never queues another's calls. Each user's calls
have a queue of their own, and the queues take turns, so one user's burst does
not hold up another's, even without a rule per user. A rule applies as soon as
it is set. To watch:

```sql
SELECT scope, limit_key, num_inbox, num_running FROM sys_vqueue_meta;
SELECT scope, l1, l2, usage, concurrency_limit FROM sys_user_limits;
```

The rules limit **concurrency**, not calls per minute. On restate-server 1.7
they need `RESTATE_EXPERIMENTAL_ENABLE_VQUEUES=true` and
`RESTATE_EXPERIMENTAL_ENABLE_PROTOCOL_V7=true` (on by default from 1.8).

## The demo

```sh
make examples-js && make gateway-demo   # or: RESTATE_SERVER=… node gateway/demo.ts in examples/
```

[`demo.ts`](demo.ts) runs a server, the outside world, and the tools,
gateway and agents in processes of their own, the agents crashing at random.
It asserts each act:

1. **No limit**: alice (acme) asks about 12 orders, 12 lookups at once. The
   order system 429s some 30 requests, the tools retry, and it takes ~17 s.
2. **A limit per tenant**: alice asks about 16, dan (globex) about 3. At most 2
   of each tenant's at once, none refused, and dan's do not wait for alice's.
3. **A limit per user**: alice asks about 6; while they queue, erin (acme too)
   looks up 2 from an MCP client. At most 1 of each user's at once, and erin's
   do not wait for alice's: `acme 2/2 (alice 1/1 +4 queued, erin 1/1)`.
4. **Approval**: bob asks for a refund. The gateway's call is held, and
   _suspended_; the agents' process is killed; the manager approves
   (`Tenant/acme/decide`). One refund.
5. **MCP**: dan's token lists only `lookup_order`, and `refund` is denied.
6. **The audit log**: `Tenant/acme/report`, one row per user and tool, and
   one tool invocation per tool call in `sys_invocation`.

## By hand

```sh
cd examples
node gateway/upstream.ts &   # the outside world, on 9300
node serve.ts gateway &      # tools, gateway, Tenant and Assistant, on 9080
node gateway/mcp.ts &        # MCP, on 9400
RESTATE_EXPERIMENTAL_ENABLE_VQUEUES=true RESTATE_EXPERIMENTAL_ENABLE_PROTOCOL_V7=true restate-server &
restate deployments register http://localhost:9080
restate rules set '*/lookup_order' --concurrency 2

curl localhost:8080/Assistant/acme.alice/prompt --json '{"id": "p1", "text": "where are orders 101 to 112?"}'
curl localhost:8080/Assistant/acme.bob/prompt --json '{"id": "p2", "text": "refund 30 of order 103"}' &
curl localhost:8080/Tenant/acme/pending --json '{}'
curl localhost:8080/Tenant/acme/decide --json '{"awakeable": "sign_…", "approved": true}'
curl localhost:8080/Tenant/acme/report --json '{}'

claude mcp add --transport http restate-tools http://127.0.0.1:9400/mcp --header "Authorization: Bearer acme.alice"
```

`PI_MODEL=anthropic/claude-sonnet-4-5` (with `ANTHROPIC_API_KEY`) replaces the
scripted model with a real one.
