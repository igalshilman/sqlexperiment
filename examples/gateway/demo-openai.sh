#!/usr/bin/env bash
# The tool gateway with a real model: the Assistant on OpenAI (PI_MODEL=openai/…),
# against a running setup (Restate on :8080/:9070, the services registered, the
# MCP endpoint on :9400). Press enter between acts.
set -euo pipefail
INGRESS=${INGRESS:-http://localhost:8080}
ADMIN=${ADMIN:-http://localhost:9070}
MCP=${MCP:-http://localhost:9400/mcp}
# A fresh user per run: an agent's session keeps the model it began with.
RUN=${RUN:-$(printf '%03x' $((RANDOM % 4096)))}

bold() { printf '\n\033[1m%s\033[0m\n' "$*"; }
dim() { printf '\033[2m%s\033[0m\n' "$*"; }
pause() { read -r -p $'\n\033[2m[enter]\033[0m ' _; }
ask() { # key id text
  printf '\n\033[36m%s>\033[0m %s\n' "$1" "$3"
  curl -s "$INGRESS/Assistant/$1-$RUN/prompt" --json "$(jq -nc --arg id "$2" --arg t "$3" '{id:$id,text:$t}')" |
    jq -r '.answer // .'
}
queues() { # the tool calls of this run, per tenant and user: running and waiting
  curl -s "$ADMIN/query" -H 'accept: application/json' --json "$(jq -nc --arg r "%-$RUN" \
    '{query: "SELECT scope, limit_key, num_running, num_inbox FROM sys_vqueue_meta WHERE limit_key LIKE \u0027\($r)\u0027 ORDER BY limit_key"}')" |
    jq -r '[.rows[]? | select(.num_running + .num_inbox > 0)] |
      if length == 0 then "  (none)" else .[] | "  \(.scope) \(.limit_key): \(.num_running) running, \(.num_inbox) waiting" end'
}

bold "1. Tools are just handlers"
dim "Restate services whose metadata names a tool; each tenant sees what its policy allows."
for t in acme globex; do
  printf '%s: ' "$t"
  curl -s "$INGRESS/Gateway/$t/tools" --json '{}' | jq -r '[.[].name] | join(", ")'
done
pause

bold "2. An agent calls them through the gateway"
ask acme.alice a1 "Hi, can you check on orders 101, 102 and 103 for me? What's the status of each?"
pause

bold "3. Limits without code: 2 lookups at once per tenant, 1 per user"
curl -s -X PUT "$ADMIN/limits/rules" -H 'content-type: application/json' \
  -d '[{"pattern":"*/lookup_order","limits":{"concurrency":2}},{"pattern":"*/lookup_order/*","limits":{"concurrency":1}}]' -o /dev/null
dim "restate rules: */lookup_order = 2, */lookup_order/* = 1"
ask acme.alice a2 "Please look up every order from 101 to 108 and give me a short table." &
sleep 0.5
ask acme.erin e1 "Where is order 201?" &
for _ in 1 2 3 4; do sleep 1.5; dim "queues:"; queues || true; done
wait
pause

bold "4. Human in the loop: a refund waits for a manager"
ask acme.bob b1 "Order 103 arrived damaged. Please refund me \$30 for it." &
PROMPT=$!
for _ in $(seq 60); do
  held=$(curl -s "$INGRESS/Gateway/acme/pending" --json '{}')
  [ "$(jq length <<<"$held")" -gt 0 ] && break
  sleep 1
done
dim "held for approval:"
jq -c '.[] | {tool: .name, arguments, user}' <<<"$held"
dim "the call is suspended in Restate; restart the services to show nothing is lost"
docker restart gw-svc >/dev/null 2>&1 && dim "(restarted gw-svc)" || true
read -r -p $'\napprove? [Y/n] ' yn
approved=$([[ "${yn:-y}" =~ ^[Nn] ]] && echo false || echo true)
curl -s "$INGRESS/Gateway/acme/decide" \
  --json "$(jq -nc --arg a "$(jq -r '.[0].awakeable' <<<"$held")" --argjson ok "$approved" '{awakeable:$a,approved:$ok}')" >/dev/null
wait $PROMPT
pause

bold "5. Policy holds for every client"
ask globex.dan d1 "I need a refund of \$10 on order 301, please."
dim "and over MCP, as Claude Code would call it:"
curl -s "$MCP" -H 'authorization: Bearer globex.dan' \
  --json '{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"refund","arguments":{"order":"301","amount":10}}}' |
  jq -r '.result.content[0].text'
