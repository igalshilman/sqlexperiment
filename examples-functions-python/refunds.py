"""A refund that waits for a person: it asks a manager to approve it, waits
(for minutes or for days: nothing runs, and a restart changes nothing), and
records the refunds approved, in the object's SQL. The key is the customer.

    curl localhost:8080/refunds/alice/request/send --json '{"amount": 30, "reason": "arrived broken"}'
    curl localhost:8080/refunds/alice/pending                                # what it waits for
    curl localhost:8080/refunds/alice/signal --json '{"name": "approval", "value": "approved"}'
    curl localhost:8080/refunds/alice/granted
"""


async def request(refund, ctx):
    verdict = await ctx.signal("approval", refund)
    if verdict != "approved":
        return {"refunded": False, "verdict": verdict}
    _table(ctx)
    amount, reason = refund["amount"], refund["reason"]
    ctx.db.sql(t"INSERT INTO refunds (amount, reason) VALUES ({amount}, {reason})")
    return {"refunded": True}


def granted(_, ctx):
    _table(ctx)
    return ctx.db.sql(t"SELECT amount, reason FROM refunds ORDER BY rowid")


def _table(ctx):
    ctx.db.sql(t"CREATE TABLE IF NOT EXISTS refunds (amount INTEGER NOT NULL, reason TEXT NOT NULL)")
