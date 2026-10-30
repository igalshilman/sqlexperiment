# The tool gateway's endpoint. Build it from the repository's root:
#
#   docker build -f examples/advanced/gateway/Dockerfile -t tool-gateway .
#
# It serves the endpoint (Orders, Payments, Gateway, Assistant, Case) on 9080:
#
#   docker run -p 9080:9080 -e OPENAI_API_KEY tool-gateway
#
# or a part: `node advanced/gateway/main.ts agents` (the Assistant and Case), or
# `node advanced/gateway/main.ts tools` (the tools and the Gateway).

# The SQL addon, for Linux.
FROM rust:1-bookworm AS addon
WORKDIR /src
COPY packages/sql packages/sql
RUN cd packages/sql && cargo build --locked --release \
  && cp target/release/librestate_sql.so sql.node

# The workspace's packages that the examples use, laid out as in the
# repository: the pnpm workspace at its root.
FROM node:24-bookworm AS deps
RUN corepack enable
WORKDIR /app
COPY package.json pnpm-workspace.yaml pnpm-lock.yaml ./
COPY packages/sql/package.json packages/sql/index.js packages/sql/index.d.ts \
  packages/sql/
COPY --from=addon /src/packages/sql/sql.node packages/sql/
COPY packages/fns packages/fns
COPY packages/integrations packages/integrations
COPY examples/package.json examples/
RUN pnpm install --frozen-lockfile --prod --filter examples...

FROM node:24-bookworm-slim
# For the agents' bash tool: Python and git, besides Node.
RUN apt-get update && apt-get install -y --no-install-recommends python3 git ca-certificates \
  && rm -rf /var/lib/apt/lists/*
COPY --from=deps /app /app
WORKDIR /app/examples
COPY examples/advanced/gateway/*.ts advanced/gateway/
ENV PORT=9080
# The agents' model, for the Assistant and Case: give OPENAI_API_KEY.
ENV MODEL=openai/gpt-6-luna
# Each agent object's files, in a directory of its own below this: a
# sandbox's disk. `-e WORKSPACE=` leaves the agents without files.
ENV WORKSPACE=/data/workspace
RUN mkdir -p /data/workspace
EXPOSE 9080
CMD ["node", "advanced/gateway/main.ts"]
