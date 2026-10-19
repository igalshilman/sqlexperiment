// The balance of one account (the key). A charge over the balance is
// declined, and changes nothing.
//
//   curl localhost:8080/payments/alice/deposit --json 100
//   curl localhost:8080/payments/alice/charge --json 30    # 70 left
let balance = 0;

export function deposit(amount) {
  return (balance += amount);
}

export function charge(amount) {
  if (amount > balance) throw new Error(`Declined: balance ${balance}, charge ${amount}`);
  return (balance -= amount);
}

export function refund(amount) {
  return (balance += amount);
}

export function current() {
  return balance;
}
