# A tool gateway on Restate

A pi agent calls tools through one gateway per tenant, which checks policy,
holds dangerous calls for approval, and limits calls per tenant and per
user. It is one endpoint, and everything else is Restate.

```
  ┌─────────────────────────────────────────────────────────────┐
  │ Assistant/<tenant>.<user>  (agent.ts)                       │
  │ pi agent, its session in the object's SQL database          │
  └────────────────────────────┬────────────────────────────────┘
                               │ RPC: Gateway.tools, Gateway.call
                               ▼
  ┌─────────────────────────────────────────────────────────────┐
  │ Gateway/<tenant>  (gateway.ts, journaled)                   │
  │ its tools · approval · scope + limit key                    │
  └────────────────────────────┬────────────────────────────────┘
                               │ scope: tenant, limit key: tool/user
                               ▼
  ┌─────────────────────────────────────────────────────────────┐
  │ Tools  (tools.ts, journaled): Orders.lookup, Payments.refund│
  │ Support.ticket, over fake outside systems, in process       │
  └─────────────────────────────────────────────────────────────┘
```

Restate holds the rest:

- each tenant's tools, in its Gateway's state: prebuilt tools
  (`lookup_order`, `refund`, `open_ticket`), registered by name. A tenant
  can call only its tools;
- the agent's session, as the object's SQLite pages in its state;
- the held calls, each suspended on an awakeable until a manager decides;
- the limits: calls over a rule wait in Restate's queues, per tenant and per
  user.

Run the endpoint, register it, and play the demo against it:

```sh
PI_MODEL=openai/gpt-5.4-mini node serve.ts gateway
restate deployments register http://localhost:9080
curl localhost:8080/Gateway/acme/register --json '{"name":"lookup_order"}'
gateway/demo.sh
```

[`demo.sh`](demo.sh) plays four acts through Restate's ingress and admin API:

1. Tools are just handlers: registered per tenant, and a prompt that uses them.
2. Limits without code: a rule per tenant and one per user.
3. Human in the loop: a refund held until a manager approves it.
4. Policy per tenant: a tenant without the refund tool cannot refund.

`PI_MODEL` is the agent's model: by default a scripted one, which needs no
key; or a real one, such as `openai/gpt-5.4-mini` with `OPENAI_API_KEY`.
On restate-server 1.7, set `RESTATE_EXPERIMENTAL_ENABLE_VQUEUES=true` and
`RESTATE_EXPERIMENTAL_ENABLE_PROTOCOL_V7=true`.

In Docker: [`Dockerfile`](Dockerfile), published as
`ghcr.io/igalshilman/tool-gateway`, serves the endpoint on 9080.
