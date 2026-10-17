"""
A bank: an object per account, its moves in its own SQL database, and
transfers that move money exactly once. A transfer debits the account and
sends the credit in one transaction: the credit is made once the debit
commits, even if the process crashes in between. Moves have ids, so a
deposit or transfer made twice moves money once.

    uv run bank.py
    restate deployments register http://localhost:9080
    curl localhost:8080/account/alice/deposit --json '{"id": "d1", "amount": 100}'
    curl localhost:8080/account/alice/transfer --json '{"id": "t1", "to": "bob", "amount": 30}'
    curl localhost:8080/account/bob/balance
"""

from restate import TerminalError
from restate_agents import Object, serve

account = Object("account", schema="CREATE TABLE IF NOT EXISTS moves (id TEXT PRIMARY KEY, amount INTEGER NOT NULL)")


async def balance_of(db) -> int:
    [[balance]] = await db.query("SELECT coalesce(sum(amount), 0) FROM moves")
    return balance


@account.handler
async def deposit(ctx, id: str, amount: int) -> int:
    """Add money, once per id; returns the balance."""
    await ctx.db.execute("INSERT OR IGNORE INTO moves VALUES (?, ?)", [id, amount])
    return await balance_of(ctx.db)


@account.handler
async def transfer(ctx, id: str, to: str, amount: int) -> int:
    """Send money to another account, once per id; returns the balance."""
    async with ctx.transaction() as t:
        if not await t.db.query("SELECT 1 FROM moves WHERE id = ?", [id]):
            if amount > await balance_of(t.db):
                raise TerminalError(f"transfer {id}: insufficient funds")
            await t.db.execute("INSERT INTO moves VALUES (?, ?)", [id, -amount])
            t.tx.object_send(deposit, key=to, arg={"id": id, "amount": amount})  # once the debit commits
        return await balance_of(t.db)


@account.handler
async def balance(ctx) -> int:
    return await balance_of(ctx.db)


@account.handler
async def history(ctx) -> list[dict]:
    return [{"id": id, "amount": amount} for id, amount in await ctx.db.query("SELECT id, amount FROM moves")]


if __name__ == "__main__":
    serve(account)
