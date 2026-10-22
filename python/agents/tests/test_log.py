"""The conversation as clients read it (restate_agents.log): stable ids, and a cursor of what is new or changed."""

from fakes import Bank, ClaudeClient, Object, Teller
from restate_agents import claude


def assistant(bank: Bank, teller: Teller):
    obj = claude.agent("teller", client=ClaudeClient(teller), tools=[bank.restate_tool()])

    @obj.handler(message=True)
    async def compact(ctx, instructions: str) -> str:
        return await claude.compact(ctx, instructions, client=ClaudeClient(teller))

    return obj, obj.handlers["chat"].fn, obj.handlers["messages"].fn


async def test_a_cursor_gives_each_message_new_or_changed_since_once():
    obj, chat, messages = assistant(Bank(), Teller())
    alice = Object()
    start = await alice.invoke(messages, None)
    assert start == {"messages": [], "more": False, "cursor": 0}
    await alice.invoke(chat, "hello")
    first = await alice.invoke(messages, {"cursor": 0})
    assert [(m["id"], m["role"]) for m in first["messages"]] == [(0, "user"), (1, "assistant")]
    # Nothing new: nothing again, and the same cursor.
    assert await alice.invoke(messages, {"cursor": first["cursor"]}) == {**first, "messages": [], "more": False}
    await alice.invoke(chat, "move 5 from 1 to 2")
    later = await alice.invoke(messages, {"cursor": first["cursor"]})
    # The tool call came as it was made, then again with its output: once here, as it is now, with its id.
    assert [m["id"] for m in later["messages"]] == [2, 3, 4]
    [call] = later["messages"][1]["parts"]
    assert call["output"] == "moved 5 from 1 to 2"


async def test_a_page_of_the_latest_and_before_an_id_with_the_cursor_to_follow():
    obj, chat, messages = assistant(Bank(), Teller())
    alice = Object()
    for text in ("one", "two", "three"):
        await alice.invoke(chat, text)
    latest = await alice.invoke(messages, {"limit": 2})
    assert [m["id"] for m in latest["messages"]] == [4, 5] and latest["more"]
    older = await alice.invoke(messages, {"limit": 4, "before": 4})
    assert [m["id"] for m in older["messages"]] == [0, 1, 2, 3] and not older["more"]
    # The cursor of a page follows on from it: nothing yet.
    assert (await alice.invoke(messages, {"cursor": latest["cursor"]}))["messages"] == []


async def test_a_compacted_conversation_keeps_its_messages_and_follows_with_new_ids():
    obj, chat, messages = assistant(Bank(), Teller())
    alice = Object()
    await alice.invoke(chat, "hello")
    cursor = (await alice.invoke(messages, None))["cursor"]
    await alice.invoke(obj.handlers["compact"].fn, "Keep it short")
    await alice.invoke(chat, "and now?")
    since = await alice.invoke(messages, {"cursor": cursor})
    # The summary and its acknowledgment, then the next message and its answer: new ids, none of the old ones again.
    assert [m["id"] for m in since["messages"]] == [2, 3, 4, 5]
    assert since["messages"][0]["parts"][0]["text"].startswith("Our conversation so far, summarized")
    assert len((await alice.invoke(messages, None))["messages"]) == 6


async def test_a_limited_read_from_a_cursor_says_there_is_more():
    obj, chat, messages = assistant(Bank(), Teller())
    alice = Object()
    for text in ("one", "two"):
        await alice.invoke(chat, text)
    page = await alice.invoke(messages, {"cursor": 0, "limit": 3})
    assert len(page["messages"]) == 3 and page["more"]
    rest = await alice.invoke(messages, {"cursor": page["cursor"], "limit": 3})
    assert [m["id"] for m in page["messages"] + rest["messages"]] == [0, 1, 2, 3] and not rest["more"]
