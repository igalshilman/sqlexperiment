"""
A bank teller agent: its tool moves money in the bank (bank.py) with the
tool call's id as the transfer's id. A crash during the tool runs it again
with the same id, so a request moves money once, however often the process
dies.

    ANTHROPIC_API_KEY=... uv run teller.py
    restate deployments register http://localhost:9080
    curl localhost:8080/account/alice/deposit --json '{"id": "d1", "amount": 100}'
    curl localhost:8080/teller/alice/chat -d 'Please send 30 to bob'
    curl localhost:8080/account/bob/balance
"""

from bank import account, balance, transfer
from restate_agents import langgraph, serve, tool


@tool
async def send_money(ctx, call_id, to: str, amount: int) -> str:
    """Send money from the customer's account to another account."""
    left = await ctx.object_call(transfer, key=ctx.key(), arg={"id": call_id, "to": to, "amount": amount})
    return f"Sent {amount} to {to}, {left} left"


@tool
async def get_balance(ctx) -> int:
    """The customer's balance."""
    return await ctx.object_call(balance, key=ctx.key(), arg=None)


# The customer is the conversation's key: teller/alice serves alice's account.
teller = langgraph.agent("teller", system="You are a bank teller. Be brief.", tools=[send_money, get_balance])

if __name__ == "__main__":
    serve(teller, account)
