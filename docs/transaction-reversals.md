
# Transaction Reversals

## Overview

Kauri Ledger corrects posted transactions through compensating
transactions rather than modifying or deleting journal entries.

Every reversal references an existing sealed transaction and
contains the exact opposite of its accounting entries.

## Endpoint

    POST /v1/ledgers/{ledger_id}/transactions/{transaction_id}/reverse

Required headers:

    Authorization: Bearer <write-api-key>
    Idempotency-Key: <unique-request-key>
    Content-Type: application/json

The idempotency key must contain between 8 and 200 characters.

The request body must be an empty JSON object:

    {}

The server derives the reversal entries from the original
transaction. Custom entries and effective dates are not accepted
by this endpoint.

## Example

Original transaction:

| Account | Debit | Credit |
|---------|------:|-------:|
| asset:wallet | 1000 | 0 |
| asset:funding | 0 | 1000 |

Compensating transaction:

| Account | Debit | Credit |
|---------|------:|-------:|
| asset:wallet | 0 | 1000 |
| asset:funding | 1000 | 0 |

Amounts are expressed in integer XOF.

## Successful response

HTTP 201 Created.

The response contains:

- `status`: posted.
- `transaction_id`: the new reversal identifier.
- `reverses_transaction`: the original transaction identifier.
- `balances_after`: immutable account balance snapshots.

Financial amounts in balance snapshots are represented as
JSON strings.

## Idempotency

Repeating the same valid request with the same idempotency key
returns the original response bytes without updating balances
or creating additional journal entries.

A different idempotency key cannot create a second reversal
for the same source transaction.

## HTTP errors

| Status | Meaning |
|--------|---------|
| 400 | Invalid request body, route, or request parameters |
| 401 | Missing or invalid API key |
| 403 | Insufficient API key scope |
| 404 | Ledger or transaction not accessible |
| 409 | Existing reversal, accounting restriction, or contention |
| 422 | Insufficient funds or applicable idempotency conflict |
| 503 | Temporary database failure or contention |

Errors use the application's structured error response.

## Accounting guarantees

- Original transactions remain immutable.
- Reversal entries are exact opposites of source entries.
- Each source transaction may be reversed once.
- A reversal may itself be reversed.
- Invalid balance changes are rolled back.
- Duplicate requests do not update balance versions.
- Deferred constraints must succeed before COMMIT.

## Operational verification

Use the development smoke script:

    ./ops/dev/smoke.sh

Audit stored accounting state:

    DATABASE_URL="$LEDGER_ADMIN_DATABASE_URL" \
    cargo run --locked \
      -p kauri-ledger-cli \
      --bin kauri-cli -- \
      audit-ledger --format json

The smoke script performs real accounting operations against
the configured development database.

A successful run posts and reverses a test transaction. It
restores the affected account balances while retaining both
transactions in the immutable journal.

Run the script only against development ledgers with no
concurrent accounting activity.

A failed script may leave a successfully committed posting.
Inspect the reported transaction identifiers before retrying.

Never disable accounting constraints to repair a failed reversal.
