"""
Google ADK agents in Restate virtual objects: each conversation is an ADK
session, whose events are stored in the object's database as the runner
appends them::

    from restate_agents import adk, serve, tool

    @tool
    def get_weather(city: str) -> str:
        '''The current weather in a city.'''
        return f"Sunny and 21°C in {city}"

    assistant = adk.agent("assistant", instruction="Answer briefly.", tools=[get_weather])
    serve(assistant)

The usual agent is an ``LlmAgent`` with ``model`` (``google/model-id``, or
an ADK model), or else MODEL if it is Google's, or else ADK's default, with
GOOGLE_API_KEY; ``system``, its instruction; and ``tools``. Or bring an agent of your
own (sub-agents, callbacks, an output schema), or an ``App`` (plugins), whose
tools may include those here through ``tools([...])``.

The handler is not replayed: a retried attempt (a crash, a lost connection)
runs again from the start, and the session carries on from its stored events:

- The runner appends the model's tool calls to the session before they run,
  and their results before the next request. A crash during the tools runs
  the stored calls of the tools here again, with the same call ids, without
  asking the model again; ADK asks the model again for those of its own
  tools, as its resumption does (at least once).
- A tool's error fails the attempt, which Restate retries; but a
  ``restate.TerminalError``, which a retry would repeat, is the tool's
  answer to the model.
- A message is answered once per invocation: its answer is stored with it,
  and a retried attempt returns it.
- A cancelled turn (``stop``) ends: each of its cut-off tool calls, and the
  turn, are answered "Cancelled", and the next message starts afresh.
- A message steered to a turn (``steer``) joins it before the next model
  request after a tool call, as an event of the session.
"""

from __future__ import annotations

import json
import os
import time
from collections.abc import Callable, Sequence
from typing import Any

import restate_fns as restate
from google.adk.agents import BaseAgent, LlmAgent
from google.adk.apps import App
from google.adk.events import Event
from google.adk.models import BaseLlm, LlmRequest
from google.adk.plugins import BasePlugin
from google.adk.runners import Runner
from google.adk.sessions import BaseSessionService, Session
from google.adk.sessions.base_session_service import GetSessionConfig, ListSessionsResponse
from google.adk.tools import BaseTool
from google.genai import types
from pydantic import ValidationError
from restate_fns import ObjectContext

from restate_agents import harness, turns
from restate_agents.common import Transcript, configured, has_table, model_id_of
from restate_agents.object import Object
from restate_agents.tools import Tool, text_of
from restate_agents.turns import CANCELLED

__all__ = ["CANCELLED", "agent", "prompt", "tools"]

# The tables of the conversations' messages and answers, and of their sessions' events.
PROMPTS, EVENTS = "adk_prompts", "adk_events"

# The user of every session: a conversation is the object's key.
USER = "restate"


def agent(
    name: str,
    adk_agent: BaseAgent | App | Callable[[ObjectContext], BaseAgent | App] | None = None,
    *,
    model: str | BaseLlm | Callable[[ObjectContext], str | BaseLlm] | None = None,
    system: str | None = None,
    tools: Sequence[Tool | Any] = (),
    **options: Any,
) -> Object:
    """
    An agent: a virtual object ``name``, one per key (its conversation), with the handlers

    - ``chat``: a message in, the answer out; the message as plain text
      (``curl -d 'Hi'``) or as a JSON string (``curl --json '"Hi"'``);
    - ``messages``: the conversation so far, a page at a time (``MessagesRequest``);
    - ``status``, ``stop`` and ``steer``: what it is doing, and the message it
      answers; ``pending`` and ``signal``: what its tools wait for.

    The usual agent takes ``model``: ``google/model-id``, an ADK model, or a
    function of the invocation's context giving either; ``system``, its
    instruction; and
    ``tools``: ``tool``s, Restate's tools, and ADK's tools. Or ``adk_agent``:
    an agent of your own, an ``App``, or a function of the context giving one.

    ``options``: those of ``restate.VirtualObject``. Add handlers of your own
    with ``@agent.handler``.
    """
    if adk_agent is not None and (model is not None or system is not None or tools):
        raise TypeError(f"{name}: an agent takes an ADK agent, or the usual agent's model, system and tools; not both")
    if not callable(model):
        _model_of(model)

    async def answer(ctx: ObjectContext, message: str) -> str:
        if adk_agent is None:
            usual = LlmAgent(name=name, instruction=system or "", tools=_tools(tools))
            given = _model_of(await configured(model, ctx, BaseLlm))
            return await prompt(ctx, usual.model_copy(update={"model": given}) if given else usual, message)
        return await prompt(ctx, await configured(adk_agent, ctx, (BaseAgent, App)), message)

    async def transcript(ctx: ObjectContext) -> Transcript:
        transcript = Transcript()
        if not await has_table(ctx.db, EVENTS):
            return transcript
        for _, event in await _Events(ctx.db, ctx.key()).load():
            _transcribe(transcript, event)
        return transcript

    return turns.agent(name, answer, transcript, **options)


def tools(given: Sequence[Tool | Any]) -> list[Any]:
    """
    ``given`` as ADK's tools, for an agent of your own::

        LlmAgent(name="support", model="gemini-2.5-flash", tools=adk.tools([call(status), signal("approval", ...)]))

    ``given``: ``tool``s, Restate's tools, and ADK's tools (functions too), as they are.
    """
    return _tools(given)


def _tools(given: Sequence[Tool | Any]) -> list[Any]:
    return [_AdkTool(t) if isinstance(t, Tool) else t for t in given]


async def prompt(ctx: Any, agent: BaseAgent | App, text: str, *, thread: str | None = None, **run: Any) -> str:
    """
    Answer ``text`` with ``agent`` (or an ``App``), in the conversation
    ``thread`` (by default, the object's key: an ADK session of that id), and
    return the text of its final response. In a handler of your own::

        @assistant.handler
        async def summarize(ctx, topic: str) -> str:
            return await adk.prompt(ctx, summarizer, f"Summarize what we said about {topic}")

    ``run``: the options of ``Runner.run_async``, such as ``run_config``. Each
    prompt is answered once, however often an attempt crashes: the
    invocation's first prompt has its id, the next ``<id>:2``, and so on.
    """
    thread = thread or ctx.key()
    sessions = _Sessions(ctx, thread)

    async def answer(pending: harness.Prompt, _: str) -> str:
        return await _run(ctx, agent, sessions, pending, run)

    async def cancelled(pending: harness.Prompt) -> None:
        await sessions.cancel()

    async def steered() -> list[str]:
        return _steered(await sessions.events.load())

    prompts = harness.Prompts(ctx.db, PROMPTS, thread)
    return await harness.answer_once(ctx, prompts, text, answer, cancelled=cancelled, steered=steered)


async def _run(
    ctx: Any, agent: BaseAgent | App, sessions: _Sessions, pending: harness.Prompt, run: dict[str, Any]
) -> str:
    """Run ``agent`` on from the session's events to the answer of the prompt ``pending``."""
    app = agent if isinstance(agent, App) else App(name=agent.name, root_agent=agent)
    app = app.model_copy(update={"plugins": [*app.plugins, _Restate(ctx, sessions)]})
    session = await sessions.session(app.name)
    turns.taken(_steered(sessions.stored))
    asked = f"prompt:{pending.prompt}"
    message = None
    if asked not in (tag for tag, _ in sessions.stored):
        sessions.tag = asked
        message = types.Content(role="user", parts=[types.Part(text=pending.text)])
    elif (last := session.events[-1]).author != "user" and last.is_final_response() and _text(last):
        # The agent answered, and a crash came before the answer was stored.
        return _text(last)
    else:
        await _carry_on(sessions, session, app.root_agent)
    runner = Runner(app=app, session_service=sessions)
    answer = ""
    async for event in runner.run_async(user_id=USER, session_id=sessions.thread, new_message=message, **run):
        if event.author != "user" and event.is_final_response() and _text(event):
            answer = _text(event)
    return answer


async def _carry_on(sessions: _Sessions, session: Session, root: BaseAgent) -> None:
    """
    Run the stored calls of the tools here that a crash cut off, with their
    ids, and append their results; ADK asks the model again for the others.
    """
    found = {t.name: t for t in _all_tools(root) if isinstance(t, _AdkTool)}
    parts = []
    for call in _unanswered(session.events):
        if call.name in found:
            result = {"result": await found[call.name].run_tool(call.args or {}, call.id)}
            parts.append(
                types.Part(function_response=types.FunctionResponse(id=call.id, name=call.name, response=result))
            )
    if parts:
        author = next(e.author for e in reversed(session.events) if e.get_function_calls())
        await sessions.append_event(session, Event(author=author, content=types.Content(role="user", parts=parts)))


class _Sessions(BaseSessionService):
    """
    The ADK sessions of the conversation ``thread``: one, whose events are in
    the object's database, a row each, in order, each with its tag: the prompt
    that it asks (``prompt:<id>``) or the steered message that it is
    (``steer:<id>``). Its state is that of its events' state changes.
    """

    def __init__(self, ctx: Any, thread: str) -> None:
        super().__init__()
        self.ctx = ctx
        self.thread = thread
        self.events = _Events(ctx.db, thread)
        self.stored: list[tuple[str | None, Event]] = []
        # The tag of the next user event appended.
        self.tag: str | None = None

    async def session(self, app_name: str = USER) -> Session:
        self.stored = await self.events.load()
        session = Session(id=self.thread, app_name=app_name, user_id=USER, last_update_time=time.time())
        for _, event in self.stored:
            self._commit_event_to_session(session, event)
        return session

    async def create_session(
        self, *, app_name: str, user_id: str, state: dict[str, Any] | None = None, session_id: str | None = None
    ) -> Session:
        return await self.session(app_name)

    async def get_session(
        self, *, app_name: str, user_id: str, session_id: str, config: GetSessionConfig | None = None
    ) -> Session | None:
        return await self.session(app_name) if session_id == self.thread else None

    async def list_sessions(self, *, app_name: str, user_id: str | None = None) -> ListSessionsResponse:
        return ListSessionsResponse(sessions=[Session(id=self.thread, app_name=app_name, user_id=USER)])

    async def delete_session(self, *, app_name: str, user_id: str, session_id: str) -> None:
        raise restate.TerminalError("an agent's session is its conversation: reset it instead")

    async def append_event(self, session: Session, event: Event) -> Event:
        if event.partial or event.error_code:
            # A failed step (a tool's error): the attempt fails with it, and Restate retries it.
            return event
        event = await super().append_event(session, event)
        tag = None
        if event.author == "user" and self.tag is not None and not event.get_function_responses():
            tag, self.tag = self.tag, None
        await self.events.append(event, tag)
        self.stored.append((tag, event))
        return event

    async def cancel(self) -> None:
        """End a cancelled run: a result for each tool call without one, then ``CANCELLED``."""
        session = await self.session()
        author = next((e.author for e in reversed(session.events) if e.author != "user"), "agent")
        parts = [
            types.Part(function_response=types.FunctionResponse(id=c.id, name=c.name, response={"result": CANCELLED}))
            for c in _unanswered(session.events)
        ]
        if parts:
            await self.append_event(session, Event(author=author, content=types.Content(role="user", parts=parts)))
        await self.append_event(
            session, Event(author=author, content=types.Content(role="model", parts=[types.Part(text=CANCELLED)]))
        )


class _Events:
    """A session's events in the object's database."""

    def __init__(self, db: Any, thread: str) -> None:
        self.db = db
        self.thread = thread
        self.length = 0

    async def load(self) -> list[tuple[str | None, Event]]:
        if not await has_table(self.db, EVENTS):
            await self.db.execute(f"""
                CREATE TABLE IF NOT EXISTS {EVENTS} (
                  thread TEXT NOT NULL,
                  n INTEGER NOT NULL,
                  tag TEXT,
                  event TEXT NOT NULL,
                  PRIMARY KEY (thread, n)
                )
            """)
        rows = await self.db.query(f"SELECT tag, event FROM {EVENTS} WHERE thread = ? ORDER BY n", [self.thread])
        self.length = len(rows)
        return [(tag, Event.model_validate_json(event)) for tag, event in rows]

    async def append(self, event: Event, tag: str | None) -> None:
        await self.db.execute(
            f"INSERT INTO {EVENTS} VALUES (?, ?, ?, ?)",
            [self.thread, self.length, tag, event.model_dump_json(exclude_none=True)],
        )
        self.length += 1
        await turns.publish()


class _Restate(BasePlugin):
    """The run in a Restate handler: the messages steered to its turn join it before the model's next request."""

    def __init__(self, ctx: Any, sessions: _Sessions) -> None:
        super().__init__(name="restate")
        self.ctx = ctx
        self.sessions = sessions

    async def before_model_callback(self, *, callback_context: Any, llm_request: LlmRequest) -> None:
        session = callback_context.session
        if not session.events or not session.events[-1].get_function_responses():
            return None
        for steered in await turns.steering(self.ctx):
            content = types.Content(role="user", parts=[types.Part(text=harness.STEERED + steered.text)])
            self.sessions.tag = f"steer:{steered.id}"
            await self.sessions.append_event(session, Event(author="user", content=content))
            turns.taken([steered.id])
            llm_request.contents.append(content)
        return None


class _AdkTool(BaseTool):
    """A ``tool`` as ADK's, which gets its call's id."""

    def __init__(self, tool: Tool) -> None:
        super().__init__(name=tool.name, description=tool.description)
        self.tool = tool

    def _get_declaration(self) -> types.FunctionDeclaration:
        schema = self.tool.input.model_json_schema()
        return types.FunctionDeclaration(name=self.name, description=self.description, parameters_json_schema=schema)

    async def run_async(self, *, args: dict[str, Any], tool_context: Any) -> Any:
        return await self.run_tool(args, tool_context.function_call_id)

    async def run_tool(self, args: dict[str, Any], call_id: str) -> str:
        """The tool's answer: its output, or a ``restate.TerminalError``'s message."""
        try:
            return text_of(await self.tool.invoke(args, call_id))
        except ValidationError as error:
            return f"Error: {error}"
        except restate.TerminalError as error:
            if error.status_code == 409:
                raise
            return error.message


def _all_tools(agent: BaseAgent) -> list[Any]:
    """The tools of ``agent`` and its sub-agents."""
    own = list(getattr(agent, "tools", None) or [])
    return [*own, *(t for sub in agent.sub_agents for t in _all_tools(sub))]


def _unanswered(events: Sequence[Event]) -> list[types.FunctionCall]:
    """The function calls of ``events`` without a response."""
    answered = {r.id for e in events for r in e.get_function_responses()}
    return [c for e in events for c in e.get_function_calls() if c.id not in answered]


def _steered(stored: Sequence[tuple[str | None, Event]]) -> list[str]:
    """The ids of the steered messages in ``stored``."""
    return [tag.removeprefix("steer:") for tag, _ in stored if tag and tag.startswith("steer:")]


def _text(event: Event) -> str:
    """The text of an event's content, without its thoughts."""
    parts = event.content.parts if event.content and event.content.parts else []
    return "".join(p.text for p in parts if p.text and not p.thought)


def _model_of(model: str | BaseLlm | None) -> str | BaseLlm | None:
    """The model of a run: an ADK model, or ``google/model-id``'s id, or MODEL's if it is Google's."""
    if isinstance(model, BaseLlm):
        return model
    if model is None:
        named = os.environ.get("MODEL", "")
        model = named if named.startswith("google/") else None
    return model_id_of(model, "google") if model is not None else None


def _transcribe(transcript: Transcript, event: Event) -> None:
    """An event, as ``messages`` gives it."""
    if event.content is None or not event.content.parts:
        return
    role = "user" if event.author == "user" else "assistant"
    if text := _text(event):
        transcript.say(role, text.removeprefix(harness.STEERED))
    for call in event.get_function_calls():
        transcript.call(call.id or "", call.name or "", call.args)
    for response in event.get_function_responses():
        output = (response.response or {}).get("result", response.response)
        transcript.result(response.id or "", output if isinstance(output, str) else json.dumps(output))
