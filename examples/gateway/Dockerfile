# The tool gateway's endpoint. Build it from the repository's root:
#
#   docker build -f examples/gateway/Dockerfile -t tool-gateway .
#
# It serves the endpoint (Orders, Payments, Gateway, Assistant, Case) on 9080:
#
#   docker run -p 9080:9080 -e OPENAI_API_KEY tool-gateway
#
# or a part: `node gateway/main.ts agents` (the Assistant and Case), or
# `node gateway/main.ts tools` (the tools and the Gateway).
#
# Needs the patched SDK's tarball, packages/sdk/.build (`make sdk-ts`).

# The sqlite-kv addon, for Linux.
FROM rust:1-bookworm AS addon
WORKDIR /src
COPY sqlite-kv sqlite-kv
COPY packages/sql packages/sql
RUN cd packages/sql && cargo build --locked --release \
  && cp target/release/libsqlite_kv_node.so sqlite_kv.node

# The workspace's packages that the examples use.
FROM node:24-bookworm AS deps
RUN corepack enable
WORKDIR /app
COPY package.json pnpm-workspace.yaml pnpm-lock.yaml ./
COPY packages/sdk/.build/restatedev-restate-sdk-1.17.3.tgz packages/sdk/.build/
COPY packages/sql/package.json packages/sql/index.js packages/sql/index.d.ts packages/sql/
COPY --from=addon /src/packages/sql/sqlite_kv.node packages/sql/
COPY packages/integrations packages/integrations
COPY examples/package.json examples/
RUN pnpm install --frozen-lockfile --prod --filter examples...

FROM node:24-bookworm-slim
# For the agents' bash tool: Python and git, besides Node.
RUN apt-get update && apt-get install -y --no-install-recommends python3 git ca-certificates \
  && rm -rf /var/lib/apt/lists/*
COPY --from=deps /app /app
WORKDIR /app/examples
COPY examples/gateway/*.ts gateway/
ENV PORT=9080
# The agents' model, for the Assistant and Case: give OPENAI_API_KEY.
ENV PI_MODEL=openai/gpt-6-luna
# Each agent object's files, in a directory of its own below this: a
# sandbox's disk. `-e WORKSPACE=` leaves the agents without files.
ENV WORKSPACE=/data/workspace
RUN mkdir -p /data/workspace
EXPOSE 9080
CMD ["node", "gateway/main.ts"]
