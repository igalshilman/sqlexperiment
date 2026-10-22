"""
The chat agent of chat.py, on Google's Agent Development Kit (ADK). Each
key is a conversation of its own, an ADK session kept in the object's SQL
database; if the process dies mid-reply, Restate retries and the agent
carries on from the session's last event, without asking the model again.

    GOOGLE_API_KEY=... uv run chat_adk.py
    restate deployments register http://localhost:9080
    curl localhost:8080/assistant/alice/chat -d 'Hi, I am Alice. Is it warm in Lisbon?'
    curl localhost:8080/assistant/alice/chat -d 'What is my name?'
    curl localhost:8080/assistant/alice/messages

The model is Gemini 2.5 Flash, with GOOGLE_API_KEY.
"""

from restate_agents import adk, serve, tool


@tool
def get_weather(city: str) -> str:
    """The current weather in a city."""
    return f"Sunny and 21°C in {city}"  # a stand-in for a real weather API


assistant = adk.agent("assistant", model="google/gemini-2.5-flash", instruction="Answer briefly.", tools=[get_weather])

if __name__ == "__main__":
    serve(assistant)
