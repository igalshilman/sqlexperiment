"""The answer-once loop of the frameworks that keep their own transcript (restate_agents.harness)."""

from fakes import Object
from restate_agents import harness, turns


async def test_a_turn_takes_the_messages_steered_into_a_cut_off_prompt_it_finished():
    answered = []

    async def handler(ctx) -> str:
        prompts = harness.Prompts(ctx.db, "prompts", ctx.key())
        # An earlier message, cut off after a message steered to this turn went into its conversation.
        await prompts.load()
        await prompts.add("earlier", "move 5 from 1 to 2")
        await prompts.steer("earlier", ["turn:steer:1"])
        answering = turns._Answering(ctx.request().id)
        turns._answering.set(answering)

        async def run(prompt: harness.Prompt, message: str) -> str:
            answered.append(message)
            return f"answered {prompt.prompt}"

        answer = await harness.answer_once(ctx, prompts, "and now?", run)
        return f"{answer}; taken: {sorted(answering.taken)}"

    # As a retried attempt finds them: the turn does not answer the steered message again as one of its own.
    assert await Object().invoke(handler, invocation="now") == "answered now; taken: ['turn:steer:1']"
    assert answered == [harness.CARRY_ON + "move 5 from 1 to 2", "and now?"]
