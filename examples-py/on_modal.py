"""
The support agent (support.py) on Modal, as a Modal Server: Restate calls it
over HTTP/2, end to end, as the agents need (each attempt runs its handler
again from the start, so it cannot work over HTTP/1.1's request-response
mode, which is all Modal's web endpoints speak).

    make wheels                                                  # at the repository's root: dist/
    uvx modal secret create anthropic ANTHROPIC_API_KEY=...
    uvx modal deploy on_modal.py
    restate deployments register https://<workspace>--restate-support-support.us-east.modal.direct \\
        --extra-header "Modal-Key: wk-..." --extra-header "Modal-Secret: ws-..."

The Modal-Key and Modal-Secret are a proxy token's (modal.com/settings/proxy-auth-tokens):
only Restate calls the agent. Then talk to it through Restate, as support.py says.
"""

from pathlib import Path

import modal

HERE = Path(__file__).parent

image = (
    modal.Image.debian_slim(python_version="3.13")
    # The patched Restate SDK, the SQL extension and restate-agents: built here, not on PyPI.
    .add_local_dir(HERE.parent / "dist", "/wheels", copy=True)
    .run_commands("pip install /wheels/*.whl")
    .add_local_python_source("support")
)

app = modal.App("restate-support", image=image)


@app.server(
    port=9080,
    h2_enabled=True,
    min_containers=1,  # Restate needs the deployment up: a Server with no containers answers 503
    exit_grace_period=600,  # an agent's turn runs on, while a container is replaced
    secrets=[modal.Secret.from_name("anthropic")],
    env={"SQLITE_KV_CACHE_FILE": "/tmp/sqlite-kv.cache"},  # a cache: each container starts cold
)
class Support:
    @modal.enter()
    def start(self):
        from restate_agents import serve
        from support import orders, refund, support

        serve(orders, refund, support, background=True)  # on port 9080, in this process
