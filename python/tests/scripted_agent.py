"""
An agent on the scripted teller (agents/tests/fakes.py): "move 5 from 1 to 2"
moves money in the bank's accounts (bank.py), once per tool call, and
"remember <fact>" remembers it with the memory example's tool. On
LangGraph, or with AGENT=claude, on Claude. Served on PORT, alone (as in
crash.py), or with the other test services (services.py).
"""

import os
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[2] / "examples-python" / "basic"))
sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "agents" / "tests"))

from bank import transfer  # noqa: E402
from fakes import ClaudeClient, Teller  # noqa: E402
from memory import FACTS, remember  # noqa: E402
from restate_agents import claude, langgraph, serve, tool  # noqa: E402


@tool(name="transfer")
async def bank_transfer(ctx, call_id, src: int, dst: int, amount: int) -> str:
    """Move money from account acct-<src> to acct-<dst>, once per tool call."""
    move = {"id": call_id, "to": f"acct-{dst}", "amount": amount}
    left = await ctx.object_call(transfer, key=f"acct-{src}", arg=move, idempotency_key=call_id)
    return f"moved {amount} from {src} to {dst}, {left} left"


TOOLS = [bank_transfer, remember]
if os.environ.get("AGENT", "langgraph") == "claude":
    teller = claude.agent("teller", client=ClaudeClient(Teller()), tools=TOOLS, schema=FACTS)
else:
    teller = langgraph.agent("teller", model=Teller(), tools=TOOLS, schema=FACTS)


@teller.handler
async def facts(ctx) -> list[str]:
    return [fact for (fact,) in await ctx.db.query("SELECT fact FROM facts ORDER BY rowid")]


if __name__ == "__main__":
    serve(teller)
