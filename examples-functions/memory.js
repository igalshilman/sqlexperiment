// Everything in memory lives on, between calls and across waits: a class
// instance, a closure, a generator paused in the middle of its loop. Nothing
// is replayed, and nothing is saved by hand.
//
//   curl localhost:8080/memory/alice/next     # "durable", then "javascript", "is"...
//   curl localhost:8080/memory/alice/tally    # {"counts": {"durable": 1, ...}, "calls": 3}
class Tally {
  #counts = new Map();
  add(word) {
    this.#counts.set(word, (this.#counts.get(word) ?? 0) + 1);
  }
  get counts() {
    return Object.fromEntries(this.#counts);
  }
}

function* cycle(words) {
  while (true) yield* words;
}

const seen = new Tally();
const words = cycle(['durable', 'javascript', 'is', 'just', 'javascript']);
let calls = 0;
const counted =
  (f) =>
  (...args) => (calls++, f(...args));

export const next = counted(async (_, ctx) => {
  const { value } = words.next();
  await ctx.sleep(100); // memory is saved here, and restored after
  seen.add(value);
  return value;
});

export function tally() {
  return { counts: seen.counts, calls };
}
