"""
LangGraph agents in Restate virtual objects: graphs whose checkpoints are in
the object's database (checkpointer.py)::

    from restate_agents import langgraph, serve, tool

    @tool
    def get_weather(city: str) -> str:
        '''The current weather in a city.'''
        return f"Sunny and 21°C in {city}"

    assistant = langgraph.agent("assistant", tools=[get_weather])
    serve(assistant)

The usual graph is the tool-calling loop: a model node, a tools node, and
back to the model until it answers. The model is ``model`` (a LangChain chat
model, or ``anthropic/model-id``), or else MODEL, or else
anthropic/claude-sonnet-5-5, with ANTHROPIC_API_KEY. Or bring a graph of
your own: ``agent(name, graph=...)``, any ``StateGraph`` whose state has
``messages`` (``MessagesState``), with ``tool_node(tools)`` for its tools.

The handler is not replayed: a retried attempt (a crash, a lost connection)
runs again from the start, and LangGraph carries on from the last step it
stored:

- Each step is stored before the next starts (``durability="sync"``), in
  order with everything else the handler does. So the tool calls a model
  made are stored before any tool runs, and a crash during the tools resumes
  them with the same tool call ids, which a tool passes on to stay idempotent
  (its ``call_id``).
- A tool's error fails the attempt, which Restate retries (the ToolNode would
  hand it to the model as the tool's result), but a ``restate.TerminalError``,
  which a retry would repeat: that is the tool's answer to the model.
- A message is answered once per invocation: it goes into the conversation
  with the invocation's id as its own, and a retried attempt that finds it
  there finishes its run, or returns its answer.
- A cancelled turn (``stop``) ends: each of its cut-off tool calls, and the
  turn, are answered "Cancelled", and the next message starts afresh.
- A message steered to a turn (``steer``) joins its conversation after the
  next tool call (``tool_node``), and the model reads it with the results.
"""

from __future__ import annotations

import weakref
from collections.abc import Awaitable, Callable, Sequence
from typing import Annotated, Any

import restate
from langchain_anthropic import ChatAnthropic
from langchain_core.language_models import BaseChatModel
from langchain_core.messages import AIMessage, BaseMessage, HumanMessage, SystemMessage, ToolMessage
from langchain_core.runnables import RunnableConfig
from langchain_core.tools import BaseTool, InjectedToolCallId, StructuredTool
from langgraph.graph import END, START, MessagesState, StateGraph
from langgraph.prebuilt import ToolNode, tools_condition
from langgraph.types import StateUpdate
from pydantic import create_model
from restate import ObjectContext

from restate_agents import turns
from restate_agents.common import Messages, MessagesRequest, Transcript, model_id_of, model_of
from restate_agents.langgraph.checkpointer import SqlCheckpointSaver
from restate_agents.object import Object, context
from restate_agents.tools import Tool
from restate_agents.turns import CANCELLED

__all__ = ["CANCELLED", "DEFAULT_MODEL", "SqlCheckpointSaver", "agent", "langchain_tools", "prompt", "tool_node"]

DEFAULT_MODEL = "anthropic/claude-sonnet-5-5"

# Marks a prompt's message in the thread (in its additional_kwargs, which models do not read).
PROMPT = "restate_agents_prompt"

Model = str | BaseChatModel
Tools = Sequence[Tool | BaseTool | Callable[[ObjectContext], BaseTool]]
Graph = StateGraph | Callable[[ObjectContext], StateGraph]


def agent(
    name: str,
    *,
    model: Model | Callable[[ObjectContext], Model] | None = None,
    system: str | None = None,
    tools: Tools = (),
    graph: Graph | None = None,
    **options: Any,
) -> Object:
    """
    An agent: a virtual object ``name``, one per key (its thread), with the handlers

    - ``chat``: a message in, the answer out; the message as plain text
      (``curl -d 'Hi'``) or as a JSON string (``curl --json '"Hi"'``);
    - ``messages``: the conversation so far, a page at a time (``MessagesRequest``);
    - ``status``, ``stop`` and ``steer``: what it is doing, and the message it
      answers; ``pending`` and ``signal``: what its tools wait for.

    The usual graph takes ``model``: ``anthropic/model-id``, or a LangChain
    chat model that calls tools, or a function of the invocation's context
    giving either; ``system``: the system prompt; and ``tools``: ``tool``s,
    LangChain tools, or functions of the invocation's context giving them.
    Or ``graph``: a ``StateGraph`` of your own (uncompiled), or a function of
    the context giving one, whose state has ``messages``.

    ``options``: those of ``restate.VirtualObject``. Add handlers of your own
    with ``@agent.handler``.
    """
    if graph is not None and (model is not None or system is not None or tools):
        raise TypeError(f"{name}: an agent takes a graph, or the usual graph's model, system and tools; not both")
    if isinstance(model, str):
        model_id_of(model, "anthropic")
    chosen = graph if graph is not None else _usual(model, system, tools)
    obj = Object(name, **options)

    async def answer(ctx: ObjectContext, message: str) -> str:
        return await prompt(ctx, chosen if isinstance(chosen, StateGraph) else chosen(ctx), message)

    async def messages(ctx: ObjectContext, request: MessagesRequest | None = None) -> Messages:
        """The conversation so far, with the tool calls, from the thread's last checkpoint."""
        saved = await SqlCheckpointSaver(ctx.db).aget_tuple({"configurable": {"thread_id": ctx.key()}})
        transcript = Transcript()
        for m in saved.checkpoint["channel_values"].get("messages", []) if saved else []:
            if isinstance(m, HumanMessage):
                transcript.say("user", _text(m))
            elif isinstance(m, AIMessage):
                transcript.say("assistant", _text(m))
                for call in m.tool_calls:
                    transcript.call(call["id"] or "", call["name"], call["args"])
            elif isinstance(m, ToolMessage):
                transcript.result(m.tool_call_id, _text(m))
        return transcript.page(request)

    turns.chat(obj, answer)
    obj.handler("messages")(messages)
    return obj


async def prompt(ctx: Any, graph: StateGraph, text: str, *, thread: str | None = None) -> str:
    """
    Answer ``text`` with ``graph`` (uncompiled; its state has ``messages``),
    on the thread ``thread`` (by default, the object's key), and return the
    answer: the text of the last AI message. In a handler of your own::

        @assistant.handler
        async def summarize(ctx, topic: str) -> str:
            return await langgraph.prompt(ctx, graph, f"Summarize what we said about {topic}")

    Each prompt is answered once, however often an attempt crashes: the
    invocation's first prompt has its id, the next ``<id>:2``, and so on.
    """
    app = graph.compile(checkpointer=SqlCheckpointSaver(ctx.db))
    config: RunnableConfig = {"configurable": {"thread_id": thread or ctx.key()}}
    id = _next_prompt_id(ctx)
    while True:
        state = await app.aget_state(config)
        if any(task.interrupts for task in state.tasks):
            raise restate.TerminalError("LangGraph's interrupt() does not work here: wait with a signal() tool instead")
        if state.tasks:
            # A run that a crash cut off (this prompt's, or an earlier one's), with steps left: finish it first.
            await _run(app, None, config)
            continue
        messages: list[BaseMessage] = state.values.get("messages", [])
        asked = next((i for i, m in enumerate(messages) if isinstance(m, HumanMessage) and m.id == id), None)
        if asked is not None:
            # Its run: up to the next prompt.
            run = messages[asked + 1 :]
            end = next((i for i, m in enumerate(run) if _prompt(m)), len(run))
            turns.taken(m.id for m in run[:end] if isinstance(m, HumanMessage) and ":steer:" in (m.id or ""))
            return next((_text(m) for m in reversed(run[:end]) if isinstance(m, AIMessage)), "")
        await _run(app, {"messages": [HumanMessage(text, id=id, additional_kwargs={PROMPT: True})]}, config)


def tool_node(tools: Tools) -> Callable[[dict[str, Any], RunnableConfig], Awaitable[dict[str, Any]]]:
    """
    A graph's node that runs the tool calls of the last message, as
    LangGraph's ``ToolNode``, and then adds the messages steered to the turn
    (``steer``), for the model to read with the results::

        graph.add_node("tools", langgraph.tool_node([get_weather, call(status)]))

    ``tools``: ``tool``s, LangChain tools, or functions of the invocation's
    context giving them. A tool's ``restate.TerminalError`` is its answer to
    the model; any other error fails the attempt, which Restate retries.
    """

    async def run_tools(state: dict[str, Any], config: RunnableConfig) -> dict[str, Any]:
        ctx = context()
        ran = await ToolNode(_langchain_tools(tools, ctx), handle_tool_errors=_terminal).ainvoke(state, config)
        has = {m.id for m in state["messages"]}
        steered = [s for s in await turns.steering(ctx) if s.id not in has]
        turns.taken(s.id for s in steered)
        return {"messages": [*ran["messages"], *(HumanMessage(s.text, id=s.id) for s in steered)]}

    return run_tools


def langchain_tools(tools: Tools) -> list[BaseTool]:
    """
    ``tools`` as LangChain tools, for a model of your own graph to call
    (``model.bind_tools(langgraph.langchain_tools(tools))``), in a node:
    ``tool``s, Restate's tools, LangChain tools, or functions of the
    invocation's context giving them.
    """
    return _langchain_tools(tools, context())


def _usual(model: Model | Callable[[ObjectContext], Model] | None, system: str | None, tools: Tools) -> StateGraph:
    """The tool-calling loop: a model node, a tools node, and back to the model until it answers."""

    async def call_model(state: MessagesState) -> dict[str, Any]:
        ctx = context()
        chosen = _model(model, ctx)
        with_tools = chosen.bind_tools(_langchain_tools(tools, ctx)) if tools else chosen
        messages = state["messages"]
        return {"messages": [await with_tools.ainvoke([SystemMessage(system), *messages] if system else messages)]}

    graph = StateGraph(MessagesState)
    graph.add_node("model", call_model)
    graph.add_node("tools", tool_node(tools))
    graph.add_edge(START, "model")
    graph.add_conditional_edges("model", tools_condition)
    graph.add_edge("tools", "model")
    return graph


async def _run(app: Any, input: dict[str, Any] | None, config: RunnableConfig) -> None:
    """Run ``app`` on ``input`` (or on from where it stopped), each step stored before the next."""
    try:
        await app.ainvoke(input, config, durability="sync")
    except restate.TerminalError as error:
        if error.status_code == 409:
            await _cancel(app, config)
        raise


async def _cancel(app: Any, config: RunnableConfig) -> None:
    """
    End the run that was cancelled: a result for each tool call of the last
    reply that has none, then ``CANCELLED`` as the reply; and no step left to
    run, so that the next prompt starts afresh.
    """
    messages: list[BaseMessage] = (await app.aget_state(config)).values.get("messages", [])
    answered = {m.tool_call_id for m in messages if isinstance(m, ToolMessage)}
    last = next((m for m in reversed(messages) if isinstance(m, AIMessage)), None)
    cut = [call for call in (last.tool_calls if last else []) if call["id"] and call["id"] not in answered]
    closing = [*(ToolMessage(CANCELLED, tool_call_id=call["id"], status="error") for call in cut), AIMessage(CANCELLED)]
    any_node = next(iter(app.builder.nodes))
    # Clear the run's steps, add the closing messages (as any node: that schedules its next steps), clear those.
    await app.abulk_update_state(
        config, [[StateUpdate(None, END)], [StateUpdate({"messages": closing}, any_node)], [StateUpdate(None, END)]]
    )


# The prompts made in each attempt, by its context: the second prompt of an invocation is its id's ":2".
_prompts: weakref.WeakKeyDictionary[Any, int] = weakref.WeakKeyDictionary()


def _next_prompt_id(ctx: Any) -> str:
    count = _prompts[ctx] = _prompts.get(ctx, 0) + 1
    id = ctx.request().id
    return id if count == 1 else f"{id}:{count}"


def _prompt(message: BaseMessage) -> bool:
    """Whether ``message`` is a prompt, rather than a message steered to one, or a graph's own."""
    return isinstance(message, HumanMessage) and message.additional_kwargs.get(PROMPT, False)


def _text(message: BaseMessage) -> str:
    text = message.text
    return text if isinstance(text, str) else text()


def _terminal(error: restate.TerminalError) -> str:
    """A tool's terminal error, as its answer to the model; but a cancellation ends the turn."""
    if error.status_code == 409:
        raise error
    return error.message


def _langchain_tools(tools: Tools, ctx: ObjectContext) -> list[BaseTool]:
    return [_langchain(t) if isinstance(t, Tool) else t if isinstance(t, BaseTool) else t(ctx) for t in tools]


def _langchain(t: Tool) -> BaseTool:
    """``t`` as a LangChain tool, which gets its tool call's id."""
    schema = create_model(t.input.__name__, __base__=t.input, tool_call_id=(Annotated[str, InjectedToolCallId], ...))

    async def run(tool_call_id: str, **input: Any) -> Any:
        return await t.run(input, tool_call_id)

    return StructuredTool.from_function(coroutine=run, name=t.name, description=t.description, args_schema=schema)


def _model(model: Model | Callable[[ObjectContext], Model] | None, ctx: ObjectContext) -> BaseChatModel:
    chosen = model(ctx) if callable(model) and not isinstance(model, BaseChatModel) else model
    if isinstance(chosen, BaseChatModel):
        return chosen
    return ChatAnthropic(model_name=model_id_of(model_of(chosen, DEFAULT_MODEL), "anthropic"))  # type: ignore[call-arg]
