# A tool gateway on Restate

A pi agent calls tools through one gateway per tenant, which holds
dangerous calls for approval, and limits calls per tenant and per user. It is one endpoint, and everything else is Restate.

```
  ┌─────────────────────────────────────────────────────────────┐
  │ Assistant/<tenant>.<user>  (agent.ts)                       │
  │ pi agent, its session in the object's SQL database          │
  └───────┬──────────────────────────────────┬──────────────────┘
          │                                  │ "Investigate …" only:
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

Run the endpoint, register it, and play the demo against it:

```sh
PI_MODEL=openai/gpt-5.4-mini node serve.ts gateway
restate deployments register http://localhost:9080
gateway/demo.sh
```

[`demo.sh`](demo.sh) plays four acts through Restate's ingress and admin API:

1. Tools are just handlers: the prebuilt tools, and a prompt that uses them.
2. Limits without code: a rule per tenant and one per user.
3. Human in the loop: a refund held until a manager approves it.
4. Sub-agents: a prompt that begins "Investigate", and only such a prompt,
   gets an `investigate` tool, which hands each order to its case agent,
   `Case/<tenant>.<order>`: a pi agent per order, with a database of its own.
   Two orders are two objects, which run at once; another user's report on
   the same order reaches the same object, which remembers the first; and
   another tenant's order of the same number is another object, empty.

`PI_MODEL` is the agent's model: by default a scripted one, which needs no
key; or a real one, such as `openai/gpt-5.4-mini` with `OPENAI_API_KEY`.
On restate-server 1.7, set `RESTATE_EXPERIMENTAL_ENABLE_VQUEUES=true` and
`RESTATE_EXPERIMENTAL_ENABLE_PROTOCOL_V7=true`.

Or as two deployments: `node serve.ts gateway-tools` (the tools and the
Gateway: stateless, so anywhere, such as your laptop) and
`node serve.ts gateway-assistant` (the Assistant, keyed per user, and the
Case, keyed per order: in a sandbox). Register both with the same Restate server.

With `WORKSPACE` set, each agent object gets a directory of its own,
`$WORKSPACE/<service>/<key>`, and pi's coding tools over it: `read`, `write`,
`edit` and `bash`, on the local file system. The scripted model understands
`Write notes.md: …`, `Read notes.md` and `Run: ls -la`. The files are on the
disk, not in Restate: the conversation outlives a lost disk, its files do
not. `bash` runs any command, so run it in a sandbox.

In Docker: [`Dockerfile`](Dockerfile), published as
`ghcr.io/igalshilman/tool-gateway`, serves the endpoint on 9080.
