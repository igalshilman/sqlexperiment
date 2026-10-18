// Fan out: run `counter` in several scopes at once, and wait for them all.
// Programs call each other like any Restate service.
//
//   curl localhost:8080/fanout/alice/run --json '["bob", "carol", "dave"]'
async function main(scopes, ctx) {
  const counts = await Promise.all(
    scopes.map((scope) => ctx.rpc.call({ service: 'counter', handler: 'run', key: scope }, 1)),
  );
  return Object.fromEntries(scopes.map((scope, i) => [scope, counts[i]]));
}
