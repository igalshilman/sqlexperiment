// A counter per scope: adds the input to the scope's count, waits a second
// on a Restate timer, and returns the new count.
//
//   curl localhost:8080/counter/alice/run --json 20
async function main(input, ctx) {
  const n = ((await ctx.kv.get('count')) ?? 0) + input;
  await ctx.kv.set('count', n);
  await ctx.sleep(1000);
  return n;
}
