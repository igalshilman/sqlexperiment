# The software factory's endpoint. Build it from the repository's root:
#
#   docker build -f examples-typescript/advanced/factory/Dockerfile -t factory .
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
# or with your .env: `docker run --env-file examples-typescript/.env ...`. The
# image holds no secrets: .dockerignore keeps every .env out of the build.

# The SQL addon, for Linux.
FROM rust:1-bookworm AS addon
WORKDIR /src
COPY core core
COPY typescript/packages/sql typescript/packages/sql
RUN cd typescript/packages/sql && cargo build --locked --release \
  && cp target/release/libsql_node.so sql.node

# The workspace's packages that the examples use, laid out as in the
# repository: the pnpm workspace in typescript/, the examples beside it.
FROM node:24-bookworm AS deps
RUN corepack enable
WORKDIR /app/typescript
COPY typescript/package.json typescript/pnpm-workspace.yaml typescript/pnpm-lock.yaml ./
COPY typescript/packages/sql/package.json typescript/packages/sql/index.js typescript/packages/sql/index.d.ts \
  packages/sql/
COPY --from=addon /src/typescript/packages/sql/sql.node packages/sql/
COPY typescript/packages/fns packages/fns
COPY typescript/packages/integrations packages/integrations
COPY examples-typescript/package.json ../examples-typescript/
RUN pnpm install --frozen-lockfile --prod --filter examples...

FROM node:24-bookworm-slim
# For the coders' bash tool: git, and Python for what they build and test.
RUN apt-get update && apt-get install -y --no-install-recommends python3 git ca-certificates \
  && rm -rf /var/lib/apt/lists/*
COPY --from=deps /app /app
WORKDIR /app/examples-typescript
COPY examples-typescript/advanced/factory/*.ts advanced/factory/
ENV PORT=9080
# The coders' clones of the project: a volume keeps them across restarts.
ENV WORKSPACE=/data/workspace
RUN mkdir -p /data/workspace
EXPOSE 9080
CMD ["node", "advanced/factory/factory.ts"]
