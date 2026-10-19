// Stock of one product (the key is its SKU). Reserving more than is in stock
// fails, and changes nothing.
//
//   curl localhost:8080/inventory/widget/restock --json 5
//   curl localhost:8080/inventory/widget/reserve --json 2    # 3 left
let stock = 0;

export function restock(qty) {
  return (stock += qty);
}

export function reserve(qty) {
  if (qty > stock) throw new Error(`Out of stock: ${stock} left, ${qty} wanted`);
  return (stock -= qty);
}

export function release(qty) {
  return (stock += qty);
}

export function available() {
  return stock;
}
