"""
A durable chat agent on LangGraph, with a tool. Each key is a conversation
of its own, kept in the object's SQL database; if the process dies
mid-reply, Restate retries and the agent carries on from its last step.

    ANTHROPIC_API_KEY=... uv run chat.py
    restate deployments register http://localhost:9080
    curl localhost:8080/assistant/alice/chat -d 'Hi, I am Alice. Is it warm in Lisbon?'
    curl localhost:8080/assistant/alice/chat -d 'What is my name?'
    curl localhost:8080/assistant/alice/messages
"""

from restate_agents import langgraph, serve, tool


@tool
def get_weather(city: str) -> str:
    """The current weather in a city."""
    return f"Sunny and 21°C in {city}"  # a stand-in for a real weather API


assistant = langgraph.agent("assistant", tools=[get_weather])

if __name__ == "__main__":
    serve(assistant)
