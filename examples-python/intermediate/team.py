"""
A team of agents on different frameworks: an editor (the OpenAI Agents SDK) hands research
to a researcher (LangGraph) and drafting to a writer (Pydantic AI), and puts
the piece together.

Each member is an agent of its own, keyed by the editor's key and the task
(``researcher/alice.lisbon``), with its own conversation: the editor can come
back to a task, and a crash anywhere resumes where it stopped.

    OPENAI_API_KEY=... uv run intermediate/team.py
    restate deployments register http://localhost:9080
    curl localhost:8080/editor/alice/chat -d 'Write a short guide to Lisbon for a first visit.'
    curl localhost:8080/writer/alice.guide/messages   # what the editor asked the writer (the task's name may differ)
"""

from restate_agents import langgraph, openai_agents, pydantic_ai, serve, subagent

researcher = langgraph.agent(
    "researcher",
    system="You research a topic and answer with the facts that matter, as a short list.",
)

writer = pydantic_ai.agent(
    "writer",
    system="You write clear, warm prose from the notes you are given. Keep it short.",
)

editor = openai_agents.agent(
    "editor",
    system="You edit a small team: research with the researcher, draft with the writer, then answer with the piece.",
    tools=[
        subagent(researcher, description="a researcher, who answers with facts"),
        subagent(writer, description="a writer, who drafts prose from notes"),
    ],
)

if __name__ == "__main__":
    serve(editor, researcher, writer)
