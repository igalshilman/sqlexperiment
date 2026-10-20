"""A Restate server, and the services of services.py registered with it, for all the tests here."""

from collections.abc import Iterator
from pathlib import Path

import pytest
from harness import Server, Service

HERE = Path(__file__).resolve().parent


@pytest.fixture(scope="session", autouse=True)
def restate_server(tmp_path_factory) -> Iterator[None]:
    logs = tmp_path_factory.mktemp("logs")
    server = Server(logs / "server.log")
    service = Service([HERE / "services.py"], 9181, logs / "services.log")
    try:
        server.start()
        service.start()
        service.register()
        yield
    finally:
        service.stop()
        server.close()
