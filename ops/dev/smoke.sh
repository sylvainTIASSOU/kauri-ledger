#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"

BASE_URL="${BASE_URL:-http://127.0.0.1:3030}"
KEY_DIR="${KEY_DIR:-$HOME/.config/kauri-ledger/dev}"

LEDGER_A="019bb000-0000-7000-8000-000000000001"
LEDGER_B="019bb000-0000-7000-8000-000000000002"

REQUEST_BODY="$ROOT/ops/dev/requests/fund-wallet.json"

if [[ ! -s "$KEY_DIR/write.key" || ! -s "$KEY_DIR/read.key" ]]; then
    echo "Missing development API keys" >&2
    exit 1
fi

WRITE_KEY="$(cat "$KEY_DIR/write.key")"
READ_KEY="$(cat "$KEY_DIR/read.key")"

TMP_DIR="$(mktemp -d)"
trap 'rm -rf "$TMP_DIR"' EXIT

POST_URL="$BASE_URL/v1/ledgers/$LEDGER_A/transactions"

BALANCE_URL="$BASE_URL/v1/ledgers/$LEDGER_A/accounts/asset:wallet/balance"

FOREIGN_URL="$BASE_URL/v1/ledgers/$LEDGER_B/accounts/asset:wallet/balance"

IDEMPOTENCY_KEY="smoke-$(openssl rand -hex 12)"

assert_status() {
    local actual="$1"
    local expected="$2"
    local scenario="$3"

    if [[ "$actual" != "$expected" ]]; then
        echo "FAIL: $scenario expected $expected, got $actual" >&2
        exit 1
    fi

    echo "PASS: $scenario ($actual)"
}

echo "Checking API health..."

curl --fail --silent --show-error \
    "$BASE_URL/health" \
    | jq -e '.is_healthy == true' >/dev/null

echo "PASS: API health"

echo "Posting a balanced transaction..."

status="$(curl -sS \
    -o "$TMP_DIR/posted.json" \
    -w '%{http_code}' \
    -X POST "$POST_URL" \
    -H "Authorization: Bearer $WRITE_KEY" \
    -H "Idempotency-Key: $IDEMPOTENCY_KEY" \
    -H "Content-Type: application/json" \
    --data-binary @"$REQUEST_BODY")"

assert_status "$status" "201" "Transaction creation"

jq -e '.status == "posted"' \
    "$TMP_DIR/posted.json" >/dev/null

echo "Replaying the same transaction..."

status="$(curl -sS \
    -o "$TMP_DIR/replayed.json" \
    -w '%{http_code}' \
    -X POST "$POST_URL" \
    -H "Authorization: Bearer $WRITE_KEY" \
    -H "Idempotency-Key: $IDEMPOTENCY_KEY" \
    -H "Content-Type: application/json" \
    --data-binary @"$REQUEST_BODY")"

assert_status "$status" "201" "Idempotent replay"

cmp -s "$TMP_DIR/posted.json" "$TMP_DIR/replayed.json"

echo "PASS: Identical replay bytes"

echo "Reading the wallet balance..."

status="$(curl -sS \
    -o "$TMP_DIR/balance.json" \
    -w '%{http_code}' \
    "$BALANCE_URL" \
    -H "Authorization: Bearer $READ_KEY")"

assert_status "$status" "200" "Authorized balance read"

jq -e '
    .account_code == "asset:wallet"
    and (.posted_total | tonumber) >= 1000
' "$TMP_DIR/balance.json" >/dev/null

echo "PASS: Wallet balance is consistent"

echo "Checking missing authentication..."

status="$(curl -sS \
    -o "$TMP_DIR/unauthenticated.json" \
    -w '%{http_code}' \
    "$BALANCE_URL")"

assert_status "$status" "401" "Missing API key"

echo "Checking insufficient scope..."

status="$(curl -sS \
    -o "$TMP_DIR/forbidden.json" \
    -w '%{http_code}' \
    -X POST "$POST_URL" \
    -H "Authorization: Bearer $READ_KEY" \
    -H "Idempotency-Key: smoke-forbidden-001" \
    -H "Content-Type: application/json" \
    --data-binary @"$REQUEST_BODY")"

assert_status "$status" "403" "Read-only key cannot post"

echo "Checking tenant isolation..."

status="$(curl -sS \
    -o "$TMP_DIR/foreign.json" \
    -w '%{http_code}' \
    "$FOREIGN_URL" \
    -H "Authorization: Bearer $READ_KEY")"

assert_status "$status" "404" "Foreign ledger isolation"

echo "Checking idempotency conflict..."

sed 's/"amount": 1000/"amount": 1001/g' \
    "$REQUEST_BODY" > "$TMP_DIR/changed.json"

status="$(curl -sS \
    -o "$TMP_DIR/conflict.json" \
    -w '%{http_code}' \
    -X POST "$POST_URL" \
    -H "Authorization: Bearer $WRITE_KEY" \
    -H "Idempotency-Key: $IDEMPOTENCY_KEY" \
    -H "Content-Type: application/json" \
    --data-binary @"$TMP_DIR/changed.json")"

assert_status "$status" "422" "Idempotency conflict"

echo "PASS: All Kauri Ledger smoke tests completed"