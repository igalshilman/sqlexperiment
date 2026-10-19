"""A counter per key: `add` adds to the key's count, waits a second on a
Restate timer, and returns the new count; `get` reads it.

    curl localhost:8080/counter/alice/add --json 20    # 20, a second later
    curl localhost:8080/counter/alice/get              # 20
"""


async def add(n, ctx):
    count = (await ctx.kv.get("count") or 0) + n
    await ctx.kv.set("count", count)
    await ctx.sleep(1)  # a durable timer: a crash meanwhile loses nothing
    return count


async def get(_, ctx):
    return await ctx.kv.get("count") or 0
