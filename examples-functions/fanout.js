// Fan out: add to `counter` under several keys at once, and wait for them
// all. Functions call each other like any Restate service.
//
//   curl localhost:8080/fanout/alice/run --json '["bob", "carol", "dave"]'
export async function run(keys, ctx) {
  const counts = await Promise.all(keys.map((key) => ctx.rpc.call({ service: 'counter', handler: 'add', key }, 1)));
  return Object.fromEntries(keys.map((key, i) => [key, counts[i]]));
}
