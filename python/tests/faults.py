"""
Serve a file of services, as ``python faults.py services.py``, aborting the
process before or after random requests of the objects' stores: with
ABORT_PER_MILLE, at random; and with ABORT_AT, at that request (counted from
1), so that it aborts at least once.
"""

import os
import random
import runpy
import signal
import sys
from pathlib import Path

from restate_fns.session import set_store_wrapper
from restate_sql import Store

PER_MILLE = float(os.environ.get("ABORT_PER_MILLE", "0"))
AT = int(os.environ.get("ABORT_AT", "0"))
requests = 0


def maybe_abort() -> None:
    global requests
    requests += 1
    if requests == AT or random.random() * 1000 < PER_MILLE:
        print(f"aborting at state request {requests}", file=sys.stderr, flush=True)
        # As abruptly as a crash.
        os.kill(os.getpid(), signal.SIGKILL)


class Faulty:
    """``store``, aborting around its requests."""

    def __init__(self, store: Store) -> None:
        self.store = store

    def get(self, key):
        maybe_abort()
        return self.store.get(key)

    def set(self, key, value):
        maybe_abort()
        self.store.set(key, value)
        maybe_abort()

    def clear(self, key):
        maybe_abort()
        self.store.clear(key)
        maybe_abort()

    def sync(self):
        maybe_abort()
        return self.store.sync()


if __name__ == "__main__":
    if PER_MILLE > 0 or AT > 0:
        set_store_wrapper(Faulty)
    path = Path(sys.argv[1]).resolve()
    sys.path.insert(0, str(path.parent))
    runpy.run_path(str(path), run_name="__main__")
