#!/usr/bin/env bash
# The tool gateway's demo, in four acts, against a Restate server that has
# the gateway's endpoint (`node serve.ts gateway`) registered. Everything
# goes through Restate: its ingress, and its admin API for the limits.
#
#   INGRESS=http://localhost:8080 ADMIN=http://localhost:9070 gateway/demo.sh
#
# The agent's model is the endpoint's PI_MODEL: the scripted one, or a real
# one, such as openai/gpt-5.4-mini. Press enter between acts.
set -euo pipefail
INGRESS=${INGRESS:-http://localhost:8080}
ADMIN=${ADMIN:-http://localhost:9070}
# Users of this run: an agent's session keeps the model it began with.
RUN=${RUN:-$(printf '%03x' $((RANDOM % 4096)))}

bold() { printf '\n\033[1m%s\033[0m\n' "$*"; }
dim() { printf '\033[2m%s\033[0m\n' "$*"; }
pause() { [ -t 0 ] && read -r -p $'\n\033[2m[enter]\033[0m ' _ || true; }
ask() { # key id text
  printf '\n\033[36m%s>\033[0m %s\n' "$1" "$3"
  curl -s "$INGRESS/Assistant/$1-$RUN/prompt" --json "$(jq -nc --arg id "$2" --arg t "$3" '{id:$id,text:$t}')" |
    jq -r '.answer // .'
}
queues() { # this run's tool calls that are running or waiting, per tenant and user
  curl -s "$ADMIN/query" -H 'accept: application/json' --json "$(jq -nc --arg r "%-$RUN" \
    '{query: "SELECT scope, limit_key, num_running, num_inbox FROM sys_vqueue_meta WHERE limit_key LIKE \u0027\($r)\u0027 AND num_running + num_inbox > 0"}')" |
    jq -r '.rows[]? | "  \(.scope) \(.limit_key): \(.num_running) running, \(.num_inbox) waiting"'
}

register() { # tenant tool
  curl -sf "$INGRESS/Gateway/$1/register" --json "{\"name\":\"$2\"}" >/dev/null
}

bold "1. Tools are just handlers, registered per tenant"
printf 'prebuilt: '
curl -s "$INGRESS/Gateway/acme/catalog" --json '{}' | jq -r '[.[].name] | join(", ")'
for tool in lookup_order refund open_ticket; do register acme $tool; done
register globex lookup_order
for t in acme globex; do
  printf '%s: ' "$t"
  curl -s "$INGRESS/Gateway/$t/tools" --json '{}' | jq -r '[.[].name] | sort | join(", ")'
done
ask acme.alice a1 "Where are orders 101 to 103?"
pause

bold "2. Limits without code: 2 lookups at once per tenant, 1 per user"
curl -sf -X PUT "$ADMIN/limits/rules" -H 'content-type: application/json' \
  -d '[{"pattern":"*/lookup_order","limits":{"concurrency":2}},{"pattern":"*/lookup_order/*","limits":{"concurrency":1}}]' >/dev/null
dim "rules: */lookup_order 2, */lookup_order/* 1"
ask acme.alice a2 "Where are orders 101 to 108? A short table, please." &
sleep 1
ask acme.erin e1 "Where is order 201?" &
for _ in 1 2 3; do sleep 1; dim "queues:"; queues; done
wait
pause

bold "3. Human in the loop: a refund waits for a manager"
ask acme.bob b1 "Refund 30 of order 103, it arrived damaged." &
held='[]'
while [ "$(jq length <<<"$held")" -eq 0 ]; do
  sleep 1
  held=$(curl -s "$INGRESS/Gateway/acme/pending" --json '{}')
done
dim "held, suspended in Restate:"
jq -c '.[] | {tool: .name, arguments, user}' <<<"$held"
approved=true
if [ -t 0 ]; then
  read -r -p $'\napprove? [Y/n] ' yn
  [[ "${yn:-y}" =~ ^[Nn] ]] && approved=false
fi
curl -s "$INGRESS/Gateway/acme/decide" \
  --json "$(jq -nc --arg a "$(jq -r '.[0].awakeable' <<<"$held")" --argjson ok "$approved" '{awakeable:$a,approved:$ok}')" >/dev/null
wait
pause

bold "4. Policy per tenant: globex has lookup_order, and no refund"
ask globex.dan d1 "Refund 10 of order 301, please."
