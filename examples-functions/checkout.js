// A saga: reserve every item, then charge the customer. When a step fails,
// undo the steps done so far, in reverse, and report why. The key is the
// order: checking out the same order again returns its first outcome.
//
//   curl localhost:8080/checkout/order-1/pay --json \
//     '{"customer": "alice", "items": [{"sku": "widget", "qty": 2, "price": 10}]}'
let outcome;

export async function pay({ customer, items }, ctx) {
  if (outcome) return outcome;
  const inventory = (sku, handler, qty) => ctx.rpc.call({ service: 'inventory', handler, key: sku }, qty);
  const undo = [];
  try {
    for (const { sku, qty } of items) {
      await inventory(sku, 'reserve', qty);
      undo.push(() => inventory(sku, 'release', qty));
    }
    const total = items.reduce((sum, { qty, price }) => sum + qty * price, 0);
    await ctx.rpc.call({ service: 'payments', handler: 'charge', key: customer }, total);
    outcome = { status: 'paid', total };
  } catch (error) {
    for (const step of undo.reverse()) await step();
    outcome = { status: 'cancelled', reason: error.message };
  }
  return outcome;
}
