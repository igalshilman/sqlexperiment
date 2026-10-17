"""
A durable chat agent on LangGraph: a tool-calling graph whose checkpoints
live in the object's SQL database.

Each key is a conversation of its own (``assistant/alice``,
``assistant/bob``). If the process dies mid-reply, Restate retries the call
and the graph carries on from its last step.

    ANTHROPIC_API_KEY=... uv run examples/chat.py   # in python/, after make py
    restate deployments register http://localhost:9080
    curl localhost:8080/assistant/alice/chat -d 'Hi, I am Alice. Is it warm in Lisbon?'
    curl localhost:8080/assistant/alice/chat -d 'What is my name?'
    curl localhost:8080/assistant/alice/messages   # the conversation so far
"""

from langchain_core.tools import tool
from restate_agents import langgraph, serve


@tool
def get_weather(city: str) -> str:
    """The current weather in a city."""
    # A stand-in for a real weather API.
    return f"Sunny and 21°C in {city}"


assistant = langgraph.agent("assistant", tools=[get_weather])

if __name__ == "__main__":
    serve(assistant)
