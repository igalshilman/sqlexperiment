"""
An agent on the scripted teller (agents/tests/fakes.py: "move 5 from 1 to 2"),
whose tool moves money in the bank example's accounts, through Restate, once
per tool call; and remembers facts with the memory example's tool, in its
database ("remember ..."). Served on PORT, alone (as in crash.py), or with the
other test services (services.py).
"""

import sys
from pathlib import Path
from typing import Annotated

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "examples"))
sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "agents" / "tests"))

from bank import Transfer, transfer  # noqa: E402
from fakes import Teller  # noqa: E402
from langchain_core.tools import InjectedToolCallId, tool  # noqa: E402
from memory import SCHEMA, remember  # noqa: E402
from restate import ObjectContext  # noqa: E402
from restate_agents import context, langgraph, serve  # noqa: E402


@tool("transfer")
async def bank_transfer(src: int, dst: int, amount: int, tool_call_id: Annotated[str, InjectedToolCallId]) -> str:
    """Move money from account acct-<src> to acct-<dst>, once per tool call."""
    move = Transfer(id=tool_call_id, to=f"acct-{dst}", amount=amount)
    left = await context().object_call(transfer, key=f"acct-{src}", arg=move, idempotency_key=tool_call_id)
    return f"moved {amount} from {src} to {dst}, {left} left"


teller = langgraph.agent("teller", model=Teller(), tools=[bank_transfer, remember])


@teller.handler()
async def facts(ctx: ObjectContext) -> list[str]:
    await ctx.db.execute(SCHEMA)
    return [fact for [fact] in await ctx.db.query("SELECT fact FROM facts ORDER BY rowid")]


if __name__ == "__main__":
    serve(teller)
