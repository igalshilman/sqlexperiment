"""
A bank: one virtual object per account, each with its moves in its own SQL
database, and transfers between accounts that move money exactly once.

A transfer debits the source account and sends the credit to the destination
in one transaction (``ctx.transaction()``): the credit is made once the debit
commits, and only then, even if the process crashes in between. Each move is
recorded with its id, so a deposit or a transfer made twice moves money once.

    uv run examples/bank.py   # in python/, after make py
    restate deployments register http://localhost:9080
    curl localhost:8080/account/alice/deposit --json '{"id": "d1", "amount": 100}'
    curl localhost:8080/account/alice/transfer --json '{"id": "t1", "to": "bob", "amount": 30}'
    curl localhost:8080/account/bob/balance
    curl localhost:8080/account/alice/history
"""

from datetime import timedelta
from typing import TypedDict

import restate
from restate import InvocationRetryPolicy, ObjectContext, SqlDatabase
from restate_agents import serve

account = restate.VirtualObject(
    "account",
    # Each attempt resumes from the database, so a failed one is retried soon.
    invocation_retry_policy=InvocationRetryPolicy(
        initial_interval=timedelta(milliseconds=50), max_interval=timedelta(milliseconds=500), max_attempts=100_000
    ),
    **restate.sql_options,
)


class Move(TypedDict):
    """Money in (a positive amount) or out (negative), by its unique id."""

    id: str
    amount: int


class Transfer(TypedDict):
    """``amount`` to the account ``to``, by its unique id."""

    id: str
    to: str
    amount: int


async def setup(db: SqlDatabase) -> None:
    await db.execute("CREATE TABLE IF NOT EXISTS moves (id TEXT PRIMARY KEY, amount INTEGER NOT NULL)")


async def balance_of(db: SqlDatabase) -> int:
    [[balance]] = await db.query("SELECT coalesce(sum(amount), 0) FROM moves")
    return balance


@account.handler()
async def deposit(ctx: ObjectContext, move: Move) -> int:
    """Add ``move``, once per id; returns the balance."""
    if move["amount"] <= 0:
        raise restate.TerminalError(f"deposit {move['id']}: the amount must be positive")
    async with ctx.db.transaction() as db:
        await setup(db)
        await db.execute("INSERT OR IGNORE INTO moves VALUES (?, ?)", [move["id"], move["amount"]])
        return await balance_of(db)


@account.handler()
async def transfer(ctx: ObjectContext, transfer: Transfer) -> int:
    """Move money to another account, once per id; returns the balance."""
    if transfer["amount"] <= 0 or transfer["to"] == ctx.key():
        raise restate.TerminalError(f"transfer {transfer['id']}: a positive amount, to another account")
    async with ctx.transaction() as t:
        await setup(t.db)
        if not await t.db.query("SELECT 1 FROM moves WHERE id = ?", [transfer["id"]]):
            if await balance_of(t.db) < transfer["amount"]:
                raise restate.TerminalError(f"transfer {transfer['id']}: insufficient funds")
            await t.db.execute("INSERT INTO moves VALUES (?, ?)", [transfer["id"], -transfer["amount"]])
            # Made once this transaction commits: the credit, by the same id.
            t.tx.object_send(deposit, key=transfer["to"], arg=Move(id=transfer["id"], amount=transfer["amount"]))
        return await balance_of(t.db)


@account.handler()
async def balance(ctx: ObjectContext) -> int:
    await setup(ctx.db)
    return await balance_of(ctx.db)


@account.handler()
async def history(ctx: ObjectContext) -> list[Move]:
    """The account's moves, in order."""
    await setup(ctx.db)
    rows = await ctx.db.query("SELECT id, amount FROM moves ORDER BY rowid")
    return [{"id": id, "amount": amount} for [id, amount] in rows]


if __name__ == "__main__":
    serve(account)
