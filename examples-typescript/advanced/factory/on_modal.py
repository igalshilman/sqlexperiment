"""
The factory's planner on Modal, as a Modal Server: the factory's image
(Dockerfile, on ghcr.io), serving the planner alone (RESTATE_SERVE_ONLY), over
HTTP/2, end to end, as the agents need. The rest of the factory runs where it
runs, and calls the planner through Restate, as before.

    uvx modal secret create factory --from-dotenv examples-typescript/.env
    uvx modal deploy examples-typescript/advanced/factory/on_modal.py
    restate deployments register https://<workspace>--restate-factory-planner.us-east.modal.direct \\
        --extra-header "Modal-Key: wk-..." --extra-header "Modal-Secret: ws-..."

The secret is the factory's .env: the planner needs only OPENAI_API_KEY, but the
endpoint asks Linear and GitHub for their tools as it starts (env.ts), whichever
agents it serves. The Modal-Key and Modal-Secret are a proxy token's
(modal.com/settings/proxy-auth-tokens): only Restate calls the planner. Serve
the rest without it, wherever they run: RESTATE_SERVE_ONLY=linear,github,triage,signals.
"""

import socket
import subprocess
import time

import modal

PORT = 9080

image = modal.Image.from_registry(
    "ghcr.io/igalshilman/sqlexperiment/factory:latest",
    add_python="3.13",  # Modal's runtime, beside the image's Node.js
)

app = modal.App("restate-factory", image=image)


@app.server(
    port=PORT,
    h2_enabled=True,
    min_containers=1,  # Restate needs the deployment up: a Server with no containers answers 503
    startup_timeout=120,  # it asks Linear and GitHub for their tools, and clones the project, first
    exit_grace_period=600,  # a turn runs on, while a container is replaced
    secrets=[modal.Secret.from_name("factory")],
    env={"RESTATE_SERVE_ONLY": "planner", "PORT": str(PORT)},
)
class Planner:
    @modal.enter()
    def start(self):
        # The image's own command, in the directory it runs in.
        self.factory = subprocess.Popen(["node", "advanced/factory/factory.ts"], cwd="/app/examples-typescript")
        wait_for(PORT, self.factory)

    @modal.exit()
    def stop(self):
        self.factory.terminate()


def wait_for(port: int, process: subprocess.Popen, timeout: float = 120):
    """Until the endpoint takes connections on `port`; an error if it exits first."""
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if process.poll() is not None:
            raise RuntimeError(f"the factory exited, with {process.returncode}")
        try:
            socket.create_connection(("127.0.0.1", port), timeout=1).close()
            return
        except OSError:
            time.sleep(0.2)
    raise TimeoutError(f"the factory is not on port {port} after {timeout}s")
