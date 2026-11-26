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

echo "Checking read API key before posting..."

status="$(curl -sS \
    -o "$TMP_DIR/read-preflight.json" \
    -w '%{http_code}' \
    "$BALANCE_URL" \
    -H "Authorization: Bearer $READ_KEY")"

assert_status "$status" "200" "Read key preflight"

FUNDING_URL="$BASE_URL/v1/ledgers/$LEDGER_A/accounts/asset:funding/balance"

status="$(curl -sS \
    -o "$TMP_DIR/funding-before.json" \
    -w '%{http_code}' \
    "$FUNDING_URL" \
    -H "Authorization: Bearer $READ_KEY")"

assert_status "$status" "200" "Funding balance preflight"

echo "Initial accounting state captured"

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

ORIGINAL_ID="$(jq -er '.transaction_id' "$TMP_DIR/posted.json")"

echo "Posted transaction: $ORIGINAL_ID"
echo "Posting idempotency key: $IDEMPOTENCY_KEY"


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


REVERSE_URL="$POST_URL/$ORIGINAL_ID/reverse"
REVERSAL_KEY="$IDEMPOTENCY_KEY-reverse"

reverse_request() {
    local token="$1"
    local key="$2"
    local output="$3"

    curl -sS \
        -o "$output" \
        -w '%{http_code}' \
        -X POST "$REVERSE_URL" \
        -H "Authorization: Bearer $token" \
        -H "Idempotency-Key: $key" \
        -H "Content-Type: application/json" \
        --data-binary '{}'
}

echo "Checking reversal authorization..."

status="$(reverse_request \
    "$READ_KEY" \
    "$REVERSAL_KEY-forbidden" \
    "$TMP_DIR/reverse-forbidden.json")"

assert_status "$status" "403" \
    "Read-only key cannot reverse"

echo "Reversing the smoke test transaction..."
echo "Reversal idempotency key: $REVERSAL_KEY"

status="$(reverse_request \
    "$WRITE_KEY" \
    "$REVERSAL_KEY" \
    "$TMP_DIR/reversed.json")"

assert_status "$status" "201" "Transaction reversal"

jq -e \
    --arg original "$ORIGINAL_ID" \
    '.status == "posted"
     and .reverses_transaction == $original
     and .transaction_id != $original' \
    "$TMP_DIR/reversed.json" >/dev/null

REVERSAL_ID="$(jq -er \
    '.transaction_id' \
    "$TMP_DIR/reversed.json")"

echo "Reversal transaction: $REVERSAL_ID"

echo "Replaying the reversal..."

status="$(reverse_request \
    "$WRITE_KEY" \
    "$REVERSAL_KEY" \
    "$TMP_DIR/reversed-replay.json")"

assert_status "$status" "201" "Idempotent reversal replay"

cmp -s \
    "$TMP_DIR/reversed.json" \
    "$TMP_DIR/reversed-replay.json"

echo "PASS: Identical reversal replay bytes"

echo "Checking duplicate reversal protection..."

status="$(reverse_request \
    "$WRITE_KEY" \
    "$REVERSAL_KEY-duplicate" \
    "$TMP_DIR/reverse-duplicate.json")"

assert_status "$status" "409" "Duplicate reversal"

jq -e '.error.code == "ALREADY_REVERSED"' \
    "$TMP_DIR/reverse-duplicate.json" >/dev/null

echo "Checking immutable transaction responses..."

for transaction_id in "$ORIGINAL_ID" "$REVERSAL_ID"; do
    status="$(curl -sS \
        -o "$TMP_DIR/transaction-$transaction_id.json" \
        -w '%{http_code}' \
        "$POST_URL/$transaction_id" \
        -H "Authorization: Bearer $READ_KEY")"

    assert_status "$status" "200" \
        "Read committed transaction $transaction_id"
done

cmp -s \
    "$TMP_DIR/posted.json" \
    "$TMP_DIR/transaction-$ORIGINAL_ID.json"

cmp -s \
    "$TMP_DIR/reversed.json" \
    "$TMP_DIR/transaction-$REVERSAL_ID.json"

echo "PASS: Immutable historical responses"

echo "Reading final account balances..."

status="$(curl -sS \
    -o "$TMP_DIR/wallet-after.json" \
    -w '%{http_code}' \
    "$BALANCE_URL" \
    -H "Authorization: Bearer $READ_KEY")"

assert_status "$status" "200" "Final wallet balance"

status="$(curl -sS \
    -o "$TMP_DIR/funding-after.json" \
    -w '%{http_code}' \
    "$FUNDING_URL" \
    -H "Authorization: Bearer $READ_KEY")"

assert_status "$status" "200" "Final funding balance"

python3 - "$TMP_DIR" <<'PY'
import json
import sys
from pathlib import Path

directory = Path(sys.argv[1])


def load(filename):
    return json.loads((directory / filename).read_text())


posted = load("posted.json")
reversed_tx = load("reversed.json")

posted_balances = {
    item["account_code"]: item
    for item in posted["balances_after"]
}

reversed_balances = {
    item["account_code"]: item
    for item in reversed_tx["balances_after"]
}

for code, before_file, after_file, delta in [
    ("asset:wallet", "read-preflight.json", "wallet-after.json", 1000),
    ("asset:funding", "funding-before.json", "funding-after.json", -1000),
]:
    before = load(before_file)
    after = load(after_file)

    posting = posted_balances[code]
    reversal = reversed_balances[code]

    assert before["account_code"] == code
    assert after["account_code"] == code

    assert (
        int(posting["posted_total"])
        == int(before["posted_total"]) + delta
    ), f"{code}: unexpected posting delta"

    for field in (
        "posted_total",
        "reserved_total",
        "available_total",
    ):
        assert int(after[field]) == int(before[field]), (
            f"{code}: {field} was not restored"
        )

        assert int(reversal[field]) == int(after[field]), (
            f"{code}: reversal snapshot mismatch for {field}"
        )

    initial_version = int(before["balance_version"])

    assert int(posting["version"]) == initial_version + 1
    assert int(reversal["version"]) == initial_version + 2
    assert int(after["balance_version"]) == initial_version + 2

    print(f"PASS: {code} restored with exactly two balance updates")

print("PASS: Accounting conservation after posting and reversal")
PY


echo "PASS: All Kauri Ledger smoke tests completed"
