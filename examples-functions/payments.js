// The balance of one account (the scope): deposit, charge, refund. A charge
// over the balance is declined, and changes nothing.
//
//   curl localhost:8080/payments/alice/run --json '{"deposit": 100}'
//   curl localhost:8080/payments/alice/run --json '{"charge": 30}'
async function main({ deposit = 0, charge = 0, refund = 0 }, ctx) {
  const balance = ((await ctx.kv.get('balance')) ?? 0) + deposit + refund;
  if (charge > balance) throw new Error(`Declined: balance ${balance}, charge ${charge}`);
  await ctx.kv.set('balance', balance - charge);
  return balance - charge;
}
