// A counter per key, in a plain variable: each key is an object of its own,
// and its memory lives on from call to call. Nothing is saved by hand.
//
//   curl localhost:8080/counter/alice/add --json 20    # 20, a second later
//   curl localhost:8080/counter/alice/add --json 5     # 25
//   curl localhost:8080/counter/alice/get              # 25
let count = 0;

export async function add(n, ctx) {
  count += n;
  await ctx.sleep(1000); // a durable timer: a crash meanwhile loses nothing
  return count;
}

export function get() {
  return count;
}
