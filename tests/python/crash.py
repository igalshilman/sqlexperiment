"""
The bank example (examples/python/bank.py) through crashes, against a real
Restate server: its process aborts at random state requests (ABORT_PER_MILLE,
default 15; see faults.py) and is restarted, and restate-server is killed
and restarted once, while clients open accounts and transfer between them.
Each request is resent until answered, with an idempotency key, and some
are sent again after their answer.

Checks: no money is made or lost; every transfer that was answered moved
its amount once from its source to its destination, by its id; every refused
one (insufficient funds) moved nothing; and every answer to a request sent
again is the first one.

    make crash-test-py   # or RESTATE_SERVER=... uv run python tests/python/crash.py
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
ACCOUNTS = 6
OPENING = 1000
TRANSFERS = 150
CLIENTS = 4

dir = Path(tempfile.mkdtemp(prefix="sqlite-kv-python-crash-"))
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


def main() -> None:
    server = Server(dir / "server.log")
    bank = Service(
        [ROOT / "tests/python/faults.py", ROOT / "examples/python/bank.py"],
        9182,
        dir / "bank.log",
        # The page cache survives the process, so that a restarted one trusts only what was saved.
        {"SQLITE_KV_CACHE_FILE": str(dir / "bank.cache")},
    )
    supervising = threading.Event()
    try:
        campaign(server, bank, supervising)
    finally:
        supervising.clear()
        bank.stop()
        server.kill()


def campaign(server: Server, bank: Service, supervising: threading.Event) -> None:
    server.start()
    # The first start aborts early, at a random request: it aborts at least once.
    bank.start({"ABORT_PER_MILLE": PER_MILLE, "ABORT_AT": str(random.randint(20, 200))})
    bank.register()

    def supervise() -> None:
        while supervising.is_set():
            if not bank.alive():
                bank.start({"ABORT_PER_MILLE": PER_MILLE})
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

    with ThreadPoolExecutor(CLIENTS) as clients:
        pending = [clients.submit(transfer, id) for id in transfers]
        wait_until(lambda: done >= TRANSFERS // 2, timeout=300, what="half the transfers")
        print("killing restate-server", file=sys.stderr)
        server.kill()
        server.start()
        for future in pending:
            future.result()

    # The rest without aborts, for the checks.
    supervising.clear()
    supervisor.join()
    bank.stop()
    bank.start()

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

    def credited() -> bool:
        books = histories()
        return all(id in books[transfers[id]["dst"]] for id in moved)

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

    print(
        f"ok: {len(moved)} transfers moved money once and {len(refused)} were refused, "
        f"through {bank.starts - 2} aborts of the bank and a restart of restate-server"
    )


if __name__ == "__main__":
    main()
