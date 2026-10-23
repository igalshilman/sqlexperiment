"""
Durable agents and SQL in Restate virtual objects: each object (one per key)
has a SQL database of its own, ``ctx.db``, where an agent keeps its
conversation, and each message is answered once per invocation, however
often an attempt crashes.

    from restate_agents import langgraph, serve, tool

    @tool
    def get_weather(city: str) -> str:
        '''The current weather in a city.'''
        return f"Sunny and 21°C in {city}"

    assistant = langgraph.agent("assistant", system="Answer briefly.", tools=[get_weather])
    serve(assistant)
"""

from restate_agents.common import has_table
from restate_agents.object import Object, app, ask, context, serve
from restate_agents.tools import Tool, call, send, signal, subagent, tool, workflow

__all__ = [
    "Object",
    "Tool",
    "app",
    "ask",
    "call",
    "context",
    "has_table",
    "send",
    "serve",
    "signal",
    "subagent",
    "tool",
    "workflow",
]
