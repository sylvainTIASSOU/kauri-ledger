# ADR-003: Idempotence guaranteed by database unique constraints

## Status

Accepted

## Context

In environments with unstable networks, HTTP clients frequently retry requests. An application-level check ("does this key exist?") is vulnerable to race conditions.

## Decision

Idempotence is guaranteed at the database level via a composite unique constraint: `UNIQUE (ledger_id, idempotency_key)`. We will also store a hash of the request payload to detect conflicts (same key, different content = HTTP 422 error).

## Consequences

- **Positive**: Absolute guarantee against duplicates, even under extreme concurrency or service crashes.
- **Negative**: Requires proper handling of unique constraint violation errors to return an idempotent HTTP response.
