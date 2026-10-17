"""
LangGraph agents in Restate virtual objects: a tool-calling graph whose
checkpoints are in the object's database (checkpointer.py)::

    from restate_agents import langgraph, serve, tool

    @tool
    def get_weather(city: str) -> str:
        '''The current weather in a city.'''
        return f"Sunny and 21°C in {city}"

    assistant = langgraph.agent("assistant", tools=[get_weather])
    serve(assistant)

The graph is the usual tool-calling loop: a model node, a tools node, and
back to the model until it answers. The model is ``model`` (a LangChain chat
model, or ``anthropic/model-id``), or else MODEL, or else
anthropic/claude-sonnet-5-5, with ANTHROPIC_API_KEY.

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
- A message is answered once per invocation: the graph records each answer
  in its state, and a retried attempt returns it.
"""

from __future__ import annotations

from collections.abc import Callable, Sequence
from typing import Annotated, Any

import restate
from langchain_anthropic import ChatAnthropic
from langchain_core.language_models import BaseChatModel
from langchain_core.messages import AIMessage, BaseMessage, HumanMessage, SystemMessage, ToolMessage
from langchain_core.tools import BaseTool, InjectedToolCallId, StructuredTool
from langgraph.graph import END, START, MessagesState, StateGraph
from langgraph.prebuilt import ToolNode, tools_condition
from pydantic import create_model
from restate import ObjectContext

from restate_agents.common import (
    CHECK_MODEL,
    Messages,
    MessageSerde,
    MessagesRequest,
    Transcript,
    model_id_of,
    model_of,
    refused,
)
from restate_agents.langgraph.checkpointer import SqlCheckpointSaver
from restate_agents.object import Object
from restate_agents.tools import Tool

__all__ = ["DEFAULT_MODEL", "SqlCheckpointSaver", "agent"]

DEFAULT_MODEL = "anthropic/claude-sonnet-5-5"


def _merge(a: dict[str, str], b: dict[str, str]) -> dict[str, str]:
    return {**a, **b}


class State(MessagesState):
    prompt: str
    """The id of the prompt being answered: its invocation's."""
    answers: Annotated[dict[str, str], _merge]
    """The answer to each prompt, by id."""


def _text(message: BaseMessage) -> str:
    text = message.text
    return text if isinstance(text, str) else text()


def _terminal(error: restate.TerminalError) -> str:
    """A tool's terminal error, as its answer to the model (the ToolNode handles the errors this takes)."""
    return error.message


def _graph(model: BaseChatModel, tools: list[BaseTool], system: str | None):
    with_tools = model.bind_tools(tools) if tools else model

    async def call_model(state: State) -> dict[str, Any]:
        messages = state["messages"]
        return {"messages": [await with_tools.ainvoke([SystemMessage(system), *messages] if system else messages)]}

    def answer(state: State) -> dict[str, Any]:
        return {"answers": {state["prompt"]: _text(state["messages"][-1])}}

    graph = StateGraph(State)
    graph.add_node("model", call_model)
    # A tool's TerminalError is its answer to the model; any other error fails the attempt, which Restate retries.
    graph.add_node("tools", ToolNode(tools, handle_tool_errors=_terminal))
    graph.add_node("answer", answer)
    graph.add_edge(START, "model")
    graph.add_conditional_edges("model", tools_condition, {"tools": "tools", END: "answer"})
    graph.add_edge("tools", "model")
    graph.add_edge("answer", END)
    return graph


Model = str | BaseChatModel
Tools = Sequence[Tool | BaseTool | Callable[[ObjectContext], BaseTool]]


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


def agent(
    name: str,
    *,
    model: Model | Callable[[ObjectContext], Model] | None = None,
    system: str | None = None,
    tools: Tools = (),
    **options: Any,
) -> Object:
    """
    An agent: a virtual object ``name``, one per key (its thread), with the handlers

    - ``chat``: a message in, the answer out; the message as plain text
      (``curl -d 'Hi'``) or as a JSON string (``curl --json '"Hi"'``);
    - ``messages``: the conversation so far, a page at a time (``MessagesRequest``).

    ``model``: ``anthropic/model-id``, or a LangChain chat model that calls
    tools, or a function of the invocation's context giving either.
    ``system``: the system prompt. ``tools``: ``tool``s, LangChain tools, or
    functions of the invocation's context giving them. ``options``: those
    of ``restate.VirtualObject``. Add handlers of your own with
    ``@agent.handler``.
    """
    if isinstance(model, str):
        model_id_of(model, "anthropic")
    obj = Object(name, **options)
    native = [_langchain(t) if isinstance(t, Tool) else t for t in tools]

    async def respond(ctx: ObjectContext, message: str) -> str:
        app = _graph(
            _model(model, ctx),
            [t if isinstance(t, BaseTool) else t(ctx) for t in native],
            system,
        ).compile(checkpointer=SqlCheckpointSaver(ctx.db))
        config = {"configurable": {"thread_id": ctx.key()}}
        id = ctx.request().id
        while True:
            state = await app.aget_state(config)  # type: ignore[arg-type]
            answers = state.values.get("answers", {})
            if id in answers:
                return answers[id]
            # Finish an interrupted run first (this message's, after a
            # crash), then start this message's. A run is unfinished while
            # its prompt has no answer.
            prompt = state.values.get("prompt")
            interrupted = prompt is not None and prompt not in answers
            run = None if interrupted else {"messages": [HumanMessage(message)], "prompt": id}
            await app.ainvoke(run, config, durability="sync")  # type: ignore[arg-type]

    async def chat(ctx: ObjectContext, message: str) -> str:
        try:
            return await respond(ctx, message)
        except restate.TerminalError:
            raise
        except Exception as error:
            if not refused(error):
                raise
            raise restate.TerminalError(f"the model's provider refused the request: {CHECK_MODEL}. {error}") from error

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

    obj.handler("chat", accept="*/*", content_type="text/plain", input_serde=MessageSerde())(chat)
    obj.handler("messages")(messages)
    return obj
