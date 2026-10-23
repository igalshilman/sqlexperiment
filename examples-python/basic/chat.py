"""
Start here: a durable chat agent on Claude, with a tool. Each key is a
conversation of its own, kept in the object's SQL database; if the process
dies mid-reply, Restate retries and the agent carries on from the last
message it stored, without asking the model again.

    ANTHROPIC_API_KEY=... uv run chat.py
    restate deployments register http://localhost:9080
    curl localhost:8080/assistant/alice/chat -d 'Hi, I am Alice. Is it warm in Lisbon?'
    curl localhost:8080/assistant/alice/chat -d 'What is my name?'
    curl localhost:8080/assistant/alice/messages
"""

from restate_agents import claude, serve, tool


@tool
def get_weather(city: str) -> str:
    """The current weather in a city."""
    return f"Sunny and 21°C in {city}"  # a stand-in for a real weather API


assistant = claude.agent("assistant", system="Answer briefly.", tools=[get_weather])

if __name__ == "__main__":
    serve(assistant)
