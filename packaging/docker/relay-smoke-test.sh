#!/usr/bin/env bash
# Start the relay image (and, given one, the web image in front of it, as compose.yaml runs them)
# and check that agents reach it only with its token and the app's tab can open its bridge.
#
# Usage: packaging/docker/relay-smoke-test.sh [RELAY_IMAGE] [WEB_IMAGE]
#        (defaults: solidcraft-relay:test, solidcraft-web:test)
#
# Both run read-only without capabilities (as compose.yaml does).
set -euo pipefail

RELAY_IMAGE="${1:-solidcraft-relay:test}"
WEB_IMAGE="${2:-solidcraft-web:test}"
ID="solidcraft-relay-smoke-$$"
TOKEN="smoke-$(od -An -N16 -tx1 /dev/urandom | tr -d ' \n')"

cleanup() {
  docker rm -f "$ID-relay" "$ID-web" >/dev/null 2>&1 || true
  docker network rm "$ID" >/dev/null 2>&1 || true
}
trap cleanup EXIT
fail() {
  echo "error: $*" >&2
  docker logs "$ID-relay" >&2 2>&1 || true
  exit 1
}

# Without a token it does not start, and says why.
set +e
out="$(docker run --rm --read-only --cap-drop ALL "$RELAY_IMAGE" 2>&1)"
code=$?
set -e
[ "$code" = 2 ] || fail "without a token the relay exits with $code, not 2"
echo "$out" | grep -q "SOLIDCRAFT_RELAY_TOKEN is not set" || fail "without a token the relay doesn't say why: $out"

docker network create "$ID" >/dev/null
docker run -d --name "$ID-relay" --network "$ID" --network-alias relay --read-only --cap-drop ALL \
  --security-opt no-new-privileges:true -e SOLIDCRAFT_RELAY_TOKEN="$TOKEN" -p 127.0.0.1::8090 "$RELAY_IMAGE" >/dev/null
PORT="$(docker port "$ID-relay" 8090/tcp | head -n1 | sed 's/.*://')"

mcp() { # BASE AUTH BODY [SESSION]: status, then the body, of a POST to BASE/mcp
  local args=(-s -o /tmp/"$ID".body -w '%{http_code}' -X POST "$1/mcp" -H 'Content-Type: application/json' -H 'Accept: application/json, text/event-stream' --data "$3")
  [ -n "$2" ] && args+=(-H "Authorization: Bearer $2")
  [ -n "${4:-}" ] && args+=(-H "Mcp-Session-Id: $4" -H 'MCP-Protocol-Version: 2025-06-18')
  curl "${args[@]}"
}

check_mcp() { # BASE: the MCP endpoint at BASE answers as it should
  local base="$1" init='{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"smoke","version":"1"}}}'
  [ "$(mcp "$base" "" "$init")" = 401 ] || fail "$base/mcp without a token is not 401"
  [ "$(mcp "$base" "wrong-token-0123456789abcdef" "$init")" = 401 ] || fail "$base/mcp with a wrong token is not 401"
  local session
  session="$(curl -s -D - -o /dev/null -X POST "$base/mcp" -H "Authorization: Bearer $TOKEN" -H 'Content-Type: application/json' --data "$init" \
    | tr -d '\r' | awk -F': ' 'tolower($1) == "mcp-session-id" { print $2 }')"
  [ -n "$session" ] || fail "$base/mcp: initialize gave no Mcp-Session-Id"
  [ "$(mcp "$base" "$TOKEN" '{"jsonrpc":"2.0","method":"notifications/initialized"}' "$session")" = 202 ] || fail "$base/mcp: a notification is not 202"
  [ "$(mcp "$base" "$TOKEN" '{"jsonrpc":"2.0","id":2,"method":"tools/list"}' "$session")" = 200 ] || fail "$base/mcp: tools/list failed"
  grep -q '"name":"execute"' /tmp/"$ID".body || fail "$base/mcp: tools/list has no execute"
  mcp "$base" "$TOKEN" '{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"execute","arguments":{"command":"solid.box"}}}' "$session" >/dev/null
  grep -q 'no SolidCraft tab is connected' /tmp/"$ID".body || fail "$base/mcp: without a tab the call doesn't say so: $(cat /tmp/"$ID".body)"
  rm -f /tmp/"$ID".body
}

BASE="http://127.0.0.1:$PORT"
for _ in $(seq 1 30); do
  curl -fs -o /dev/null "$BASE/healthz" && break
  sleep 1
done
curl -fs "$BASE/healthz" | grep -q ok || fail "the relay is not healthy"
docker exec "$ID-relay" /usr/local/bin/solidcraft-relay --health || fail "the image's health check fails"
check_mcp "$BASE"
echo "relay smoke test passed: $RELAY_IMAGE"

# Behind the web app's nginx, as compose.yaml runs them.
if docker image inspect "$WEB_IMAGE" >/dev/null 2>&1; then
  docker run -d --name "$ID-web" --network "$ID" --read-only --tmpfs /tmp --cap-drop ALL -p 127.0.0.1::8080 "$WEB_IMAGE" >/dev/null
  WEB="http://127.0.0.1:$(docker port "$ID-web" 8080/tcp | head -n1 | sed 's/.*://')"
  for _ in $(seq 1 30); do
    curl -fs -o /dev/null "$WEB/" && break
    sleep 1
  done
  check_mcp "$WEB"
  # The tab's bridge: the WebSocket upgrade goes through.
  status="$(curl -s -o /dev/null -w '%{http_code}' --max-time 3 --http1.1 "$WEB/bridge" -H "Origin: $WEB" -H 'Connection: Upgrade' \
    -H 'Upgrade: websocket' -H 'Sec-WebSocket-Version: 13' -H 'Sec-WebSocket-Key: c29saWRjcmFmdC1zbW9rZSE=' || true)"
  [ "$status" = 101 ] || fail "the bridge upgrade through nginx answers $status, not 101"
  status="$(curl -s -o /dev/null -w '%{http_code}' --max-time 3 --http1.1 "$WEB/bridge" -H 'Origin: https://elsewhere.example' -H 'Connection: Upgrade' \
    -H 'Upgrade: websocket' -H 'Sec-WebSocket-Version: 13' -H 'Sec-WebSocket-Key: c29saWRjcmFmdC1zbW9rZSE=' || true)"
  [ "$status" = 403 ] || fail "a page from another site opening the bridge gets $status, not 403"
  echo "relay smoke test passed: $WEB_IMAGE passes /mcp and /bridge on to $RELAY_IMAGE"
fi
