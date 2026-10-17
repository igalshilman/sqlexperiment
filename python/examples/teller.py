"""
A bank teller agent on LangGraph, whose tool moves money between the bank's
accounts (bank.py): a Restate call, with the tool call's id as its
idempotency key. A crash during the tool resumes it with the same id, so
however often the process dies, a request moves money once.

    ANTHROPIC_API_KEY=... uv run examples/teller.py   # in python/, after make py
    restate deployments register http://localhost:9080
    curl localhost:8080/account/alice/deposit --json '{"id": "d1", "amount": 100}'
    curl localhost:8080/teller/alice/chat -d 'Please send 30 from my account to bob'
    curl localhost:8080/account/bob/balance
"""

from typing import Annotated

from bank import Transfer, account, balance, transfer
from langchain_core.tools import InjectedToolCallId, tool
from restate_agents import context, langgraph, serve


@tool
async def send_money(to: str, amount: int, tool_call_id: Annotated[str, InjectedToolCallId]) -> str:
    """Send money from the customer's account to another account."""
    ctx = context()
    # The customer is the conversation's key: teller/alice is alice's. A
    # refusal (insufficient funds) is a TerminalError: the tool's answer.
    move = Transfer(id=tool_call_id, to=to, amount=amount)
    left = await ctx.object_call(transfer, key=ctx.key(), arg=move, idempotency_key=tool_call_id)
    return f"Sent {amount} to {to}; {left} left"


@tool
async def get_balance() -> int:
    """The customer's balance."""
    ctx = context()
    return await ctx.object_call(balance, key=ctx.key(), arg=None)


teller = langgraph.agent(
    "teller",
    system="You are a bank teller. The customer is the owner of the account you serve. Answer briefly.",
    tools=[send_money, get_balance],
)

if __name__ == "__main__":
    serve(teller, account)
