"""
Agent frameworks in Restate virtual objects: each object (one per key)
keeps the agent's state in its SQL database (``ctx.db``), and each message is
answered once per invocation, however often an attempt crashes.

    from restate_agents import langgraph, serve

    assistant = langgraph.agent("assistant", tools=[weather])
    serve(assistant)
"""

from restate_agents.object import context, object, serve

__all__ = ["context", "object", "serve"]
