// The balance of one account (the key). A charge over the balance is
// declined, and changes nothing.
//
//   curl localhost:8080/payments/alice/deposit --json 100
//   curl localhost:8080/payments/alice/charge --json 30    # 70 left
export async function deposit(amount, ctx) {
  return change(amount, ctx);
}

export async function charge(amount, ctx) {
  return change(-amount, ctx);
}

export async function refund(amount, ctx) {
  return change(amount, ctx);
}

export async function current(_, ctx) {
  return (await ctx.kv.get('balance')) ?? 0;
}

async function change(by, ctx) {
  const balance = (await ctx.kv.get('balance')) ?? 0;
  if (balance + by < 0) throw new Error(`Declined: balance ${balance}, charge ${-by}`);
  await ctx.kv.set('balance', balance + by);
  return balance + by;
}
