// Takes its time: waits `ms` on a timer, then answers.
//
//   curl localhost:8080/slow/alice/wait --json 2000
export async function wait(ms, ctx) {
  await ctx.sleep(ms);
  return `done after ${ms} ms`;
}
