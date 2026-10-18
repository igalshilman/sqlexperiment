// Takes its time: waits `ms` on a timer, then answers.
//
//   curl localhost:8080/slow/alice/run --json 2000
async function main(ms, ctx) {
  await ctx.sleep(ms);
  return `done after ${ms} ms`;
}
