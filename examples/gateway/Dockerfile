# The tool gateway's endpoint. Build it from the repository's root:
#
#   docker build -f examples/gateway/Dockerfile -t tool-gateway .
#
# It serves the endpoint (Orders, Payments, Gateway, Assistant) on 9080:
#
#   docker run -p 9080:9080 -e PI_MODEL=openai/gpt-5.4-mini -e OPENAI_API_KEY tool-gateway
#
# Needs the patched SDK's tarball, vendor/typescript/.build (`make sdk-ts`).

# The sqlite-kv addon, for Linux.
FROM rust:1-bookworm AS addon
WORKDIR /src
COPY sqlite-kv sqlite-kv
RUN cd sqlite-kv/node && cargo build --locked --release \
  && cp target/release/libsqlite_kv_node.so sqlite_kv.node

FROM node:24-bookworm-slim
WORKDIR /app
COPY sqlite-kv/node/package.json sqlite-kv/node/index.js sqlite-kv/node/index.d.ts sqlite-kv/node/
COPY --from=addon /src/sqlite-kv/node/sqlite_kv.node sqlite-kv/node/
COPY vendor/typescript/.build/restatedev-restate-sdk-1.17.3.tgz vendor/typescript/.build/
COPY examples/package.json examples/
WORKDIR /app/examples
# Only what the gateway needs, at the examples' versions.
RUN node -e ' \
  const fs = require("fs"); \
  const p = JSON.parse(fs.readFileSync("package.json")); \
  const keep = ["@restatedev/restate-sdk", "sqlite-kv", "zod", "@earendil-works/chord", "@earendil-works/pi-ai", "@earendil-works/pi-durable"]; \
  p.dependencies = Object.fromEntries(keep.map((k) => [k, p.dependencies[k]])); \
  delete p.devDependencies; delete p.scripts; \
  fs.writeFileSync("package.json", JSON.stringify(p, null, 2));' \
  && npm install --omit=dev --no-audit --no-fund
COPY examples/serve.ts examples/faults.ts ./
COPY examples/pi/utils.ts examples/pi/with-pi.ts examples/pi/pi-storage.ts pi/
COPY examples/gateway/*.ts gateway/
ENV PORT=9080
EXPOSE 9080
CMD ["node", "serve.ts", "gateway"]
