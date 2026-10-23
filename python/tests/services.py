"""
The services of the tests against a Restate server: the notes example, a
bank, a probe of the context without journal replay, the agent on the
scripted teller (scripted_agent.py), and a support agent with Restate's
tools, on each framework; a desk of your own that asks the helper; and a
graph whose steps are Restate's. Served on PORT, as the examples are.
"""

import asyncio
import os
import sys
from datetime import timedelta
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[2] / "examples-python" / "basic"))
sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "agents" / "tests"))

import restate_fns as restate  # noqa: E402
from bank import account  # noqa: E402
from fakes import (
    AdkTeller,
    AgentsTeller,
    ClaudeClient,
    LiteLLMTeller,
    MessagesServer,
    ResponsesServer,
    Teller,
    pydantic_ai_model,
)  # noqa: E402
from langchain_core.messages import AIMessage  # noqa: E402
from langgraph.graph import START, MessagesState, StateGraph  # noqa: E402
from notes import add, notes  # noqa: E402
from pydantic import BaseModel  # noqa: E402
from restate_agents import (  # noqa: E402
    Object,
    adk,
    ask,
    call,
    claude,
    claude_agent,
    codex,
    langgraph,
    litellm,
    openai_agents,
    pydantic_ai,
    send,
    serve,
    signal,
    subagent,
    workflow,
)
from restate_agents.langgraph import nodes  # noqa: E402
from restate_fns import Context, ObjectContext, SqlTransaction, WorkflowContext  # noqa: E402
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
probe = restate.VirtualObject("probe", inactivity_timeout=timedelta(seconds=1))


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
async def stepped(ctx: ObjectContext) -> dict:
    """A step commits once: the retried attempt gets its result, and its SQL is there once."""

    async def first(t: SqlTransaction) -> int:
        await t.db.execute("CREATE TABLE IF NOT EXISTS stepped (id INTEGER PRIMARY KEY)")
        await t.db.execute("INSERT INTO stepped DEFAULT VALUES")
        [[rows]] = await t.db.query("SELECT count(*) FROM stepped")
        return rows

    attempt = await ctx.step("first", first)
    [[rows]] = await ctx.db.query("SELECT count(*) FROM stepped")
    await ctx.db.execute("CREATE TABLE IF NOT EXISTS tries (id INTEGER PRIMARY KEY)")
    await ctx.db.execute("INSERT INTO tries DEFAULT VALUES")
    [[tries]] = await ctx.db.query("SELECT count(*) FROM tries")
    if tries < 2:
        raise RuntimeError("once more")
    return {"first": attempt, "rows": rows, "tries": tries}


@probe.handler()
async def has_steps(ctx: ObjectContext) -> bool:
    """Whether the table of steps exists."""
    return bool(await ctx.db.query("SELECT 1 FROM sqlite_schema WHERE name = 'restate_steps'"))


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
async def with_run(ctx: ObjectContext) -> str:
    """A run runs its action, again on each attempt."""
    return await ctx.run_typed("greet", lambda: "hello")


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


orders = restate.Service("orders")


@orders.handler()
async def status(ctx: Context, order: str) -> str:
    """The status of an order."""
    if order != "103":
        raise restate.TerminalError(f"no order {order}")
    return f"order {order}: delivered"


class Refund(BaseModel):
    order: str
    amount: float


refund = restate.Workflow("refund")


@refund.main()
async def run(ctx: WorkflowContext, request: Refund) -> str:
    """Refund an order."""
    await ctx.sleep(timedelta(milliseconds=200))
    return f"refunded {request.order}"


helper = langgraph.agent("helper", model=Teller())

# The scripted model's "use <tool> <json> then ..." calls these in turn.
SUPPORT_TOOLS = [
    call(status),
    signal("approval", description="Wait for a manager to approve a refund.", input=Refund),
    workflow(refund, description="Refund an order, once a manager approved it."),
    subagent(helper, description="a helper"),
    send(add, description="Add a note to someone's notes."),
]
support = langgraph.agent("support", model=Teller(), tools=SUPPORT_TOOLS)
pydantic_support = pydantic_ai.agent("pydantic_support", model=pydantic_ai_model(Teller()), tools=SUPPORT_TOOLS)
claude_support = claude.agent("claude_support", client=ClaudeClient(Teller()), tools=SUPPORT_TOOLS)
# Claude Code's CLI calls the teller as the Messages API, with none of a Claude Code session that runs the tests.
for name in [n for n in os.environ if n.startswith(("CLAUDE", "ANTHROPIC"))]:
    del os.environ[name]
messages_api = MessagesServer(Teller()).start()
claude_agent_support = claude_agent.agent(
    "claude_agent_support",
    tools=SUPPORT_TOOLS,
    claude_options={"env": {"ANTHROPIC_BASE_URL": messages_api.url, "ANTHROPIC_API_KEY": "sk-teller"}},
)
# Codex calls the teller as the Responses API, a model provider of its configuration.
os.environ["TELLER_API_KEY"] = "sk-teller"
codex_support = codex.agent("codex_support", tools=SUPPORT_TOOLS, config=ResponsesServer(Teller()).start().config)
# The OpenAI Agents SDK, on the teller as its model, sending no traces.
os.environ["OPENAI_AGENTS_DISABLE_TRACING"] = "1"
agents_support = openai_agents.agent("agents_support", model=AgentsTeller(Teller()), tools=SUPPORT_TOOLS)
adk_support = adk.agent("adk_support", model=AdkTeller(Teller()), tools=SUPPORT_TOOLS)
LiteLLMTeller(Teller()).register()
litellm_support = litellm.agent("litellm_support", model="teller/teller", tools=SUPPORT_TOOLS)

# A Claude assistant that compacts and resets its conversation, by handlers of its own.
notebook_client = ClaudeClient(Teller())
notebook = claude.agent("notebook", client=notebook_client)


@notebook.handler(message=True)
async def compact(ctx: ObjectContext, instructions: str) -> str:
    return await claude.compact(ctx, instructions, client=notebook_client)


@notebook.handler(message=True)
async def reset(ctx: ObjectContext, handoff: str) -> None:
    await claude.reset(ctx, handoff)


# A desk of your own, which sends each message to the helper (ask).
desk = Object("desk")


@desk.chat
async def desk_chat(ctx: ObjectContext, message: str) -> str:
    return await ask(ctx, helper, message)


class RefundState(MessagesState):
    status: str
    approval: str
    refunded: str


def order(state: RefundState) -> str:
    """The order the message is about: the message itself."""
    return state["messages"][0].text


# Restate's steps of a graph, rather than tools a model chooses: the order's status, an approval, the refund, a note.
steps = StateGraph(RefundState)
steps.add_node("status", nodes.call(status, input=order, update="status"))
steps.add_node("approval", nodes.signal("approval", input=lambda state: {"order": order(state)}, update="approval"))
steps.add_node(
    "refund", nodes.workflow(refund, input=lambda state: Refund(order=order(state), amount=5), update="refunded")
)
steps.add_node(
    "note", nodes.send(add, key=lambda state: f"refunds-{order(state)}", input=lambda state: state["refunded"])
)
steps.add_node(
    "answer",
    lambda state: {"messages": [AIMessage(" / ".join([state["status"], state["approval"], state["refunded"]]))]},
)
steps.add_edge(START, "status")
for a, b in [("status", "approval"), ("approval", "refund"), ("refund", "note"), ("note", "answer")]:
    steps.add_edge(a, b)
refunds = langgraph.agent("refunds", graph=steps)


if __name__ == "__main__":
    serve(
        echo,
        probe,
        notes,
        account,
        teller,
        orders,
        refund,
        helper,
        support,
        pydantic_support,
        claude_support,
        claude_agent_support,
        codex_support,
        agents_support,
        adk_support,
        litellm_support,
        desk,
        notebook,
        refunds,
    )
