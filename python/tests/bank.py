"""
The tests' bank: an object per account, its moves in its own SQL database,
and transfers that move money exactly once. A transfer debits the account
and sends the credit in one transaction: the credit is made once the debit
commits, even if the process crashes in between. Moves have ids, so a
deposit or transfer made twice moves money once. Served alone in crash.py,
and with the other test services in services.py.
"""

from restate import TerminalError
from restate_agents import serve
from restate_agents.object import Object

account = Object("account")


async def moves(db):
    await db.execute("CREATE TABLE IF NOT EXISTS moves (id TEXT PRIMARY KEY, amount INTEGER NOT NULL)")
    return db


async def balance_of(db) -> int:
    [[balance]] = await db.query("SELECT coalesce(sum(amount), 0) FROM moves")
    return balance


@account.handler
async def deposit(ctx, id: str, amount: int) -> int:
    """Add money, once per id; returns the balance."""
    db = await moves(ctx.db)
    await db.execute("INSERT OR IGNORE INTO moves VALUES (?, ?)", [id, amount])
    return await balance_of(db)


@account.handler
async def transfer(ctx, id: str, to: str, amount: int) -> int:
    """Send money to another account, once per id; returns the balance."""
    async with ctx.transaction() as t:
        await moves(t.db)
        if not await t.db.query("SELECT 1 FROM moves WHERE id = ?", [id]):
            if amount > await balance_of(t.db):
                raise TerminalError(f"transfer {id}: insufficient funds")
            await t.db.execute("INSERT INTO moves VALUES (?, ?)", [id, -amount])
            t.tx.object_send(deposit, key=to, arg={"id": id, "amount": amount})  # once the debit commits
        return await balance_of(t.db)


@account.handler
async def balance(ctx) -> int:
    return await balance_of(await moves(ctx.db))


@account.handler
async def history(ctx) -> list[dict]:
    db = await moves(ctx.db)
    return [{"id": id, "amount": amount} for id, amount in await db.query("SELECT id, amount FROM moves")]


if __name__ == "__main__":
    serve(account)
