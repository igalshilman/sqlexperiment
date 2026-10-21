"""
A bank (bank.py) and an agent moving money in it (scripted_agent.py, on
LangGraph, or with AGENT=claude on Claude; on the scripted teller: "move 5
from 1 to 2"), through
crashes, against a real Restate server: their processes abort at random
state requests (see faults.py; the bank's at ABORT_PER_MILLE, default 15,
the agent's at AGENT_ABORT_PER_MILLE, default 3, as a prompt makes some
hundreds of them) and are restarted, and restate-server is killed and
restarted once, while clients open accounts and transfer between them, and
prompt the agent. Each request is resent
until answered, with an idempotency key, and some are sent again after their
answer.

Checks: no money is made or lost; every transfer that was answered moved
its amount once from its source to its destination, by its id; every refused
one (insufficient funds) moved nothing; every answer to a request sent again
is the first one; and every agent prompt moved money exactly once (or, refused,
not at all), and is in its conversation once.

    make crash-test-py   # or AGENT=claude; or, in python/: RESTATE_SERVER=... uv run python tests/crash.py
"""

import os
import random
import sys
import tempfile
import threading
import time
from concurrent.futures import ThreadPoolExecutor
from pathlib import Path
from typing import Any

from harness import ROOT, Server, Service, call, wait_until

PER_MILLE = os.environ.get("ABORT_PER_MILLE", "15")
AGENT_PER_MILLE = os.environ.get("AGENT_ABORT_PER_MILLE", "3")
ACCOUNTS = 6
OPENING = 1000
TRANSFERS = 150
CLIENTS = 4
SESSIONS = 3
PROMPTS = 6

dir = Path(tempfile.mkdtemp(prefix="journal-python-crash-"))
print(f"logs in {dir}", file=sys.stderr)


def answered(path: str, body: Any, key: str) -> tuple[int, Any]:
    """POST until answered, through crashes of either side: 200, or a failure of the invocation."""
    deadline = time.monotonic() + 300
    while time.monotonic() < deadline:
        status, answer = call(path, body, {"idempotency-key": key})
        if status == 200:
            return status, answer
        if status not in (0, 404) and (
            status < 500 or (isinstance(answer, dict) and answer.get("source") == "invocation")
        ):
            return status, answer
        time.sleep(0.2)
    raise TimeoutError(f"{path} {key}: no answer")


def faulty(path: str, port: int, name: str) -> Service:
    """``path``'s services, aborting at random state requests (faults.py)."""
    # The page cache survives the process, so that a restarted one trusts only what was saved.
    cache = {"JOURNAL_CACHE_FILE": str(dir / f"{name}.cache")}
    return Service([ROOT / "python/tests/faults.py", ROOT / path], port, dir / f"{name}.log", cache)


def main() -> None:
    server = Server(dir / "server.log")
    bank = faulty("python/tests/bank.py", 9182, "bank")
    agent = faulty("python/tests/scripted_agent.py", 9183, "agent")
    supervising = threading.Event()
    try:
        campaign(server, {bank: PER_MILLE, agent: AGENT_PER_MILLE}, supervising)
    finally:
        supervising.clear()
        bank.stop()
        agent.stop()
        server.close()


def campaign(server: Server, services: dict[Service, str], supervising: threading.Event) -> None:
    server.start()
    for service, per_mille in services.items():
        # The first start aborts early, at a random request: each aborts at least once.
        service.start({"ABORT_PER_MILLE": per_mille, "ABORT_AT": str(random.randint(20, 200))})
        service.register()

    def supervise() -> None:
        while supervising.is_set():
            for service, per_mille in services.items():
                if not service.alive():
                    service.start({"ABORT_PER_MILLE": per_mille})
            time.sleep(0.05)

    supervising.set()
    supervisor = threading.Thread(target=supervise, daemon=True)
    supervisor.start()

    accounts = [f"acct-{i}" for i in range(ACCOUNTS)]
    for account in accounts:
        status, _ = answered(f"account/{account}/deposit", {"id": "opening", "amount": OPENING}, f"open-{account}")
        assert status == 200, f"opening {account}: {status}"

    transfers: dict[str, dict[str, Any]] = {}
    for i in range(TRANSFERS):
        src, dst = random.sample(accounts, 2)
        transfers[f"t{i}"] = {"src": src, "dst": dst, "amount": random.randint(1, 400)}
    outcomes: dict[str, tuple[int, Any]] = {}
    done = 0
    lock = threading.Lock()

    def transfer(id: str) -> None:
        nonlocal done
        t = transfers[id]
        body = {"id": id, "to": t["dst"], "amount": t["amount"]}
        outcome = answered(f"account/{t['src']}/transfer", body, id)
        if int(id[1:]) % 10 == 0:
            # Sent again: the same invocation, the same answer.
            again = answered(f"account/{t['src']}/transfer", body, id)
            assert again == outcome, f"{id}: answered {outcome}, then {again}"
        with lock:
            outcomes[id] = outcome
            done += 1

    # The agent's prompts: each with an amount of its own, which tells its moves apart.
    prompts = {
        (session, i): (*random.sample(range(ACCOUNTS), 2), 1 + session * PROMPTS + i)
        for session in range(SESSIONS)
        for i in range(PROMPTS)
    }
    answers: dict[tuple[int, int], Any] = {}

    def converse(session: int) -> None:
        for i in range(PROMPTS):
            src, dst, amount = prompts[(session, i)]
            status, answer = answered(
                f"teller/s{session}/chat", f"move {amount} from {src} to {dst}", f"s{session}-{i}"
            )
            assert status == 200, f"prompt {session}/{i}: {status} {answer}"
            with lock:
                answers[(session, i)] = answer

    with ThreadPoolExecutor(CLIENTS + SESSIONS) as clients:
        # The conversations first: they run alongside the transfers, through the restart too.
        pending = [clients.submit(converse, session) for session in range(SESSIONS)]
        pending += [clients.submit(transfer, id) for id in transfers]
        wait_until(lambda: done >= TRANSFERS // 2, timeout=600, what="half the transfers")
        print("killing restate-server", file=sys.stderr)
        server.kill()
        server.start()
        for future in pending:
            future.result()

    # The rest without aborts, for the checks.
    supervising.clear()
    supervisor.join()
    for service in services:
        service.stop()
        service.start()

    moved = {id for id, (status, _) in outcomes.items() if status == 200}
    refused = {id for id, (status, answer) in outcomes.items() if status != 200}
    for id in refused:
        status, answer = outcomes[id]
        assert status == 500 and "insufficient funds" in answer["message"], f"{id}: {status} {answer}"

    def histories() -> dict[str, dict[str, int]]:
        result = {}
        for account in accounts:
            status, moves = answered(f"account/{account}/history", None, f"history-{account}-{time.monotonic()}")
            assert status == 200
            result[account] = {move["id"]: move["amount"] for move in moves}
            assert len(result[account]) == len(moves), f"{account}: a move recorded twice"
        return result

    # The agent's moves: the ones neither an opening nor a transfer of the clients.
    def agent_moves(books: dict[str, dict[str, int]]) -> list[tuple[str, int]]:
        return [
            (account, amount)
            for account, moves in books.items()
            for id, amount in moves.items()
            if id != "opening" and id not in transfers
        ]

    prompted = {key: prompts[key] for key, answer in answers.items() if not answer.endswith("insufficient funds")}

    def credited() -> bool:
        books = histories()
        return all(id in books[transfers[id]["dst"]] for id in moved) and len(agent_moves(books)) == 2 * len(prompted)

    # The credits are sends, made once their debits commit.
    wait_until(credited, timeout=120, what="every credit")
    books = histories()

    total = sum(sum(moves.values()) for moves in books.values())
    assert total == ACCOUNTS * OPENING, f"money made or lost: {total} != {ACCOUNTS * OPENING}"
    for id, t in transfers.items():
        debit, credit = books[t["src"]].get(id), books[t["dst"]].get(id)
        if id in moved:
            assert (debit, credit) == (-t["amount"], t["amount"]), f"{id} {t}: debit {debit}, credit {credit}"
        else:
            assert (debit, credit) == (None, None), f"{id} refused, yet debit {debit}, credit {credit}"
    for account, moves in books.items():
        assert sum(moves.values()) >= 0, f"{account} is overdrawn"

    expected = sorted(
        move for src, dst, amount in prompted.values() for move in ((f"acct-{src}", -amount), (f"acct-{dst}", amount))
    )
    assert sorted(agent_moves(books)) == expected, f"the agent's moves: {sorted(agent_moves(books))} != {expected}"
    for (session, i), answer in answers.items():
        source, target, amount = prompts[(session, i)]
        assert answer.startswith(f"Done: moved {amount} from {source} to {target}") or answer.endswith(
            "insufficient funds"
        ), f"prompt {session}/{i}: {answer}"
    for session in range(SESSIONS):
        status, page = answered(f"teller/s{session}/messages", None, f"messages-{session}-{time.monotonic()}")
        said = [m["parts"][0]["text"] for m in page["messages"] if m["role"] == "user"]
        assert said == [
            f"move {amount} from {src} to {dst}" for (s, _), (src, dst, amount) in prompts.items() if s == session
        ]

    print(
        f"ok: {len(moved)} transfers moved money once and {len(refused)} were refused; "
        f"{len(prompted)} of {len(prompts)} agent prompts moved money once, the rest refused; "
        f"through {sum(service.starts - 2 for service in services)} aborts of the services "
        "and a restart of restate-server"
    )


if __name__ == "__main__":
    main()
