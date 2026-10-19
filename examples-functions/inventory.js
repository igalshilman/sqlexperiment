// Stock of one product (the scope is its SKU): restock, reserve, release.
// Reserving more than is in stock fails, and changes nothing.
//
//   curl localhost:8080/inventory/widget/run --json '{"restock": 5}'
//   curl localhost:8080/inventory/widget/run --json '{"reserve": 2}'
async function main({ restock = 0, reserve = 0, release = 0 }, ctx) {
  const stock = ((await ctx.kv.get('stock')) ?? 0) + restock + release;
  if (reserve > stock) throw new Error(`Out of stock: ${stock} left, ${reserve} wanted`);
  await ctx.kv.set('stock', stock - reserve);
  return stock - reserve;
}
