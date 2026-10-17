"""
The services of the tests against a Restate server: the examples' notes and
bank, a probe of the context without journal replay, and the agent on the
scripted teller (scripted_agent.py). Served on PORT, as the examples are.
"""

import asyncio
import sys
from datetime import timedelta
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[2] / "examples-py"))

import restate  # noqa: E402
from bank import account  # noqa: E402
from notes import notes  # noqa: E402
from restate import Context, ObjectContext  # noqa: E402
from restate_agents import serve  # noqa: E402
from scripted_agent import teller  # noqa: E402

echo = restate.Service("echo")


@echo.handler()
async def shout(ctx: Context, word: str) -> str:
    await ctx.sleep(timedelta(milliseconds=200))
    return word.upper()


@echo.handler()
async def slowly(ctx: Context, word: str) -> str:
    await ctx.sleep(timedelta(seconds=3))
    return word


# A short inactivity timeout, so that a handler waiting for a few seconds suspends.
probe = restate.VirtualObject("probe", inactivity_timeout=timedelta(seconds=1), **restate.sql_options)


@probe.handler()
async def flaky(ctx: ObjectContext) -> int:
    """Fails twice after a commit: each attempt runs again, and sees the commits of the ones before."""
    await ctx.db.execute("CREATE TABLE IF NOT EXISTS attempts (id INTEGER PRIMARY KEY)")
    await ctx.db.execute("INSERT INTO attempts DEFAULT VALUES")
    [[attempts]] = await ctx.db.query("SELECT count(*) FROM attempts")
    if attempts < 3:
        raise RuntimeError(f"attempt {attempts}: once more")
    return attempts


@probe.handler()
async def waits(ctx: ObjectContext) -> str:
    """Suspends while a call takes 3s: resumes by running again, and its call, by its key, is the same one."""
    await ctx.db.execute("CREATE TABLE IF NOT EXISTS runs (id INTEGER PRIMARY KEY)")
    await ctx.db.execute("INSERT INTO runs DEFAULT VALUES")
    word = await ctx.service_call(slowly, "done", idempotency_key=f"{ctx.request().id}:slowly")
    [[runs]] = await ctx.db.query("SELECT count(*) FROM runs")
    return f"{word} after {runs} runs"


@probe.handler()
async def fan_out(ctx: ObjectContext, words: list[str]) -> list[str]:
    """Calls made at once and awaited together."""
    calls = [ctx.service_call(shout, word, idempotency_key=f"{ctx.request().id}:{i}") for i, word in enumerate(words)]
    return list(await asyncio.gather(*calls))


@probe.handler()
async def started(ctx: ObjectContext) -> str:
    """A call starts when it is made, awaited or not: this one is awaited after a sleep longer than it."""
    call = ctx.service_call(shout, "early", idempotency_key=f"{ctx.request().id}:early")
    await ctx.sleep(timedelta(milliseconds=500))
    return await call


@probe.handler()
async def combined(ctx: ObjectContext) -> list:
    """State and a sleep, with asyncio's combinators."""
    count, _ = await asyncio.gather(ctx.get("count", type_hint=int), ctx.sleep(timedelta(milliseconds=10)))
    ctx.set("count", (count or 0) + 1)
    done, pending = await asyncio.wait(
        [
            asyncio.ensure_future(ctx.sleep(timedelta(seconds=10))),
            asyncio.ensure_future(ctx.get("count", type_hint=int)),
        ],
        return_when=asyncio.FIRST_COMPLETED,
    )
    return [count, len(done), len(pending)]


@probe.handler()
async def run_refused(ctx: ObjectContext) -> None:
    await ctx.run_typed("nothing", lambda: None)


@probe.handler()
async def awakeable_refused(ctx: ObjectContext) -> None:
    ctx.awakeable()


@probe.handler()
async def sql_error(ctx: ObjectContext) -> None:
    await ctx.db.query("SELECT * FROM missing")


@probe.handler()
async def cancellable(ctx: ObjectContext) -> None:
    """Waits for two long sleeps: a cancellation reaches both, and what it does after that runs normally."""
    try:
        await asyncio.gather(ctx.sleep(timedelta(seconds=60)), ctx.sleep(timedelta(seconds=60)))
    except restate.TerminalError:
        await ctx.db.execute("CREATE TABLE IF NOT EXISTS cancelled (n INTEGER)")
        await ctx.db.execute("INSERT INTO cancelled VALUES (1)")
        raise


@probe.handler()
async def cancellations(ctx: ObjectContext) -> int:
    await ctx.db.execute("CREATE TABLE IF NOT EXISTS cancelled (n INTEGER)")
    [[n]] = await ctx.db.query("SELECT count(*) FROM cancelled")
    return n


if __name__ == "__main__":
    serve(echo, probe, notes, account, teller)
