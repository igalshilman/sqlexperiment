# The software factory's endpoint. Build it from the repository's root:
#
#   docker build -f packages/examples/advanced/factory/Dockerfile -t factory .
#
# It serves the webhooks (linear, github) and the agents (triage, planner,
# coder, reviewer) on 9080, on the real Linear and GitHub (README.md, "Run it,
# on Linear and GitHub"):
#
#   docker run -p 9080:9080 -v factory:/data \
#     -e LINEAR_API_KEY -e LINEAR_WEBHOOK_SECRET \
#     -e GITHUB_REPO -e GITHUB_TOKEN -e GITHUB_WEBHOOK_SECRET \
#     -e OPENAI_API_KEY -e ANTHROPIC_API_KEY factory
#
# or with your .env: `docker run --env-file packages/examples/.env ...`. The
# image holds no secrets: .dockerignore keeps every .env out of the build.

# The SQL addon, for Linux.
FROM rust:1-bookworm AS addon
WORKDIR /src
COPY packages/objects/native packages/objects/native
RUN cd packages/objects/native && cargo build --locked --release \
  && cp target/release/librestate_session_objects.so objects.node

# The workspace's packages that the examples use, laid out as in the
# repository: the pnpm workspace at its root.
FROM node:24-bookworm AS deps
RUN corepack enable
WORKDIR /app
COPY package.json pnpm-workspace.yaml pnpm-lock.yaml ./
COPY packages/objects packages/objects
# Into the package of the image's platform (packages/objects/src/platform.ts).
COPY --from=addon /src/packages/objects/native/objects.node /tmp/objects.node
RUN mv /tmp/objects.node packages/objects/npm/linux-$(node -p process.arch)-gnu/
COPY packages/agents packages/agents
COPY packages/examples/package.json packages/examples/
# With development dependencies: restate-agents' harnesses are its optional
# peers, installed in the workspace as its development dependencies.
RUN pnpm install --frozen-lockfile --filter examples...

FROM node:24-bookworm-slim
# For the coders' bash tool: git, and Python for what they build and test.
RUN apt-get update && apt-get install -y --no-install-recommends python3 git ca-certificates \
  && rm -rf /var/lib/apt/lists/*
COPY --from=deps /app /app
WORKDIR /app/packages/examples
COPY packages/examples/advanced/factory/*.ts advanced/factory/
# Node's compile cache, made here: the modules the endpoint loads as it starts
# (restate-agents, OpenCode's and pi's, many files) come compiled already, a
# quarter less of their loading on a new container, such as a coder's sandbox.
# Every module but env.ts, which asks Linear and GitHub as it loads.
ENV NODE_COMPILE_CACHE=/app/.node-compile-cache
RUN node --input-type=module -e "for (const m of ['restate-agents', 'restate-agents/opencode', 'restate-agents/pi', \
  './advanced/factory/answers.ts', './advanced/factory/github.ts', './advanced/factory/mcp.ts', \
  './advanced/factory/prompts.ts', './advanced/factory/signatures.ts', './advanced/factory/workspace.ts']) await import(m)"
ENV PORT=9080
# The coders' clones of the project: a volume keeps them across restarts.
ENV WORKSPACE=/data/workspace
RUN mkdir -p /data/workspace
EXPOSE 9080
CMD ["node", "advanced/factory/factory.ts"]
