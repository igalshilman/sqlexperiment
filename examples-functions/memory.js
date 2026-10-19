// The whole heap survives a wait: a class instance, a closure, a generator
// paused in the middle of its loop, all as they were. Nothing is replayed.
//
//   curl localhost:8080/memory/alice/run --json 3
class Tally {
  #seen = new Map();
  add(word) {
    this.#seen.set(word, (this.#seen.get(word) ?? 0) + 1);
  }
  get counts() {
    return Object.fromEntries(this.#seen);
  }
}

function* words() {
  yield* ['durable', 'javascript', 'is', 'durable'];
}

export async function run(rounds, ctx) {
  const tally = new Tally();
  const next = words();
  let calls = 0;
  const count = () => ++calls;
  for (let i = 0; i < rounds; i++) {
    const { value, done } = next.next();
    if (done) break;
    tally.add(value);
    count();
    await ctx.sleep(100); // the heap is saved here, and restored after
  }
  return { tally: tally.counts, calls };
}
