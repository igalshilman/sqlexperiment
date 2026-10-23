"""
The agent of chat.py on each framework: the same definition, the same tool,
the same handlers (``chat``, ``messages``, ``steer``...), whichever
framework runs the loop. Pick the one you use; each keeps its conversation
in its object's SQL database, in the framework's own format.

    ANTHROPIC_API_KEY=... OPENAI_API_KEY=... GOOGLE_API_KEY=... uv run basic/frameworks.py
    restate deployments register http://localhost:9080
    curl localhost:8080/on_claude/alice/chat -d 'Is it warm in Lisbon?'
    curl localhost:8080/on_langgraph/alice/chat -d 'Is it warm in Lisbon?'
    curl localhost:8080/on_openai_agents/alice/chat -d 'Is it warm in Lisbon?'

Each needs only its own provider's key: Claude, LiteLLM, LangGraph and
Pydantic AI take Anthropic's, the OpenAI Agents SDK OpenAI's, ADK Google's.
"""

from restate_agents import adk, claude, langgraph, litellm, openai_agents, pydantic_ai, serve, tool


@tool
def get_weather(city: str) -> str:
    """The current weather in a city."""
    return f"Sunny and 21°C in {city}"  # a stand-in for a real weather API


SYSTEM = "Answer briefly."

agents = [
    claude.agent("on_claude", system=SYSTEM, tools=[get_weather]),
    litellm.agent("on_litellm", model="anthropic/claude-sonnet-5-5", system=SYSTEM, tools=[get_weather]),
    langgraph.agent("on_langgraph", system=SYSTEM, tools=[get_weather]),
    pydantic_ai.agent("on_pydantic_ai", system=SYSTEM, tools=[get_weather]),
    openai_agents.agent("on_openai_agents", model="openai/gpt-5-mini", system=SYSTEM, tools=[get_weather]),
    adk.agent("on_adk", model="google/gemini-2.5-flash", system=SYSTEM, tools=[get_weather]),
]

if __name__ == "__main__":
    serve(*agents)
