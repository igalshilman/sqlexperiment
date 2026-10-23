"""The conversation as a person reads it (restate_agents.client.as_text)."""

from restate_agents.client import as_text


def test_a_message_is_its_text_and_each_tool_call_on_a_line_of_its_own():
    said = as_text(
        {
            "id": 3,
            "role": "assistant",
            "parts": [
                {"type": "text", "text": "Moving it."},
                {"type": "tool", "id": "c1", "name": "transfer", "input": {"amount": 5}, "output": "moved 5"},
                {"type": "tool", "id": "c2", "name": "audit", "input": {}},
            ],
        }
    )
    assert said == 'assistant: Moving it.\n  transfer({"amount": 5}) -> moved 5\n  audit({}) -> …'
