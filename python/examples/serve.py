"""Serve Restate services over HTTP/2, as the examples do: on PORT, or 9080."""

import asyncio
import os

import hypercorn.asyncio
import hypercorn.config
import restate


def serve(*services: restate.Service | restate.VirtualObject | restate.Workflow) -> None:
    """Serve ``services`` until interrupted."""
    config = hypercorn.config.Config()
    config.bind = [f"0.0.0.0:{os.environ.get('PORT', '9080')}"]
    asyncio.run(hypercorn.asyncio.serve(restate.app(list(services)), config))  # type: ignore[arg-type]
