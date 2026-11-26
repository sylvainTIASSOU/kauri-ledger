
# ADR-004: Unified Transaction Posting and Exact Reversals

## Status

Accepted for Lot C implementation.

Extends ADR-003 for the multi-tenant accounting engine.

## Context

Kauri Ledger supports trusted internal postings, tenant-authorized
HTTP postings, and compensating transactions.

Separate posting implementations introduce duplicated accounting
logic and potentially inconsistent database lock acquisition orders.

Financial corrections must preserve the original journal while
preventing duplicate reversals, unauthorized access, overdrafts,
and inconsistent account balances.

## Decision

All supported multi-tenant posting operations delegate to the
private PostgreSQL function `ledger.execute_posting`.

The following functions remain the supported entry points:

- `ledger.post_transaction`
- `ledger.authorized_post_transaction`
- `ledger.authorized_reverse_transaction`

Their existing signatures are preserved.

### Idempotency

The logical operation identity is:

    (ledger_id, route_scope, idempotency_key)

A database unique constraint enforces uniqueness.

Requests are identified by a SHA-256 hash of the HTTP method,
canonical path, and canonical JSON payload.

A transaction-level advisory lock serializes requests with the
same logical operation identity before balance locks are acquired.

Matching requests replay the original stored response bytes.
A different request hash for an existing identity is rejected.

### Locking protocol

Supported posting paths acquire locks in the following order:

1. API authorization locks, where applicable.
2. Operation-specific advisory lock.
3. Parent ledger lock.
4. Original transaction lock for reversals.
5. Account balance locks in ascending account UUID order.

An existing idempotent result is resolved before mutable
accounting state is evaluated.

### Accounting updates

Repeated entries affecting the same account are aggregated
before cumulative account totals are updated.

HTTP response snapshots are generated from the database values
returned by `UPDATE ... RETURNING`.

Journal entries, balance updates, response bytes, and transaction
relationships are persisted within one database transaction.

Success is returned only after COMMIT.

### Reversal semantics

A reversal is a new immutable transaction linked to its source
through `reverses_transaction`.

Every source entry must have an exact opposite entry with the
same account, currency, amount, and multiplicity.

The source transaction remains unchanged.

Only one reversal is permitted for each source transaction.
A reversal may itself be reversed.

Reversals remain subject to ordinary balance constraints,
account status, ledger status, and accounting period restrictions.

### Database security

The HTTP runtime uses a restricted PostgreSQL role.

Authorized wrappers execute with trusted ownership and fixed
search paths. They validate tenant access before entering the
private posting engine.

Direct execution of the private engine and direct access to
accounting tables are denied to the runtime role.

Legacy single-tenant storage remains isolated from the runtime.

### Independent integrity checks

Deferred constraints verify balanced transactions and exact
reversal relationships before commit.

Transaction seals prevent late additions to committed journals.

The audit command independently checks stored accounting totals,
journal conservation, seals, and reversal relationships.

## Consequences

### Positive

- One accounting implementation for supported posting paths.
- Consistent lock acquisition across those paths.
- Exact response replay without repeated financial mutations.
- Auditable corrections without rewriting the original journal.
- Database-enforced accounting and tenant boundaries.

### Trade-offs

- Additional PostgreSQL lock management.
- Contention for operations affecting shared accounts.
- Historical response snapshots consume database storage.
- PostgreSQL-specific functionality reduces database portability.
- The private engine becomes a critical component requiring
  thorough integration testing.

### Limitations

Advisory locks do not replace database uniqueness constraints.

Hash collisions may introduce unnecessary serialization but
cannot identify an unrelated stored operation.

The locking protocol reduces known deadlock risks; it does not
guarantee that every future administrative or privileged
database workflow will be deadlock-free.

Trusted internal posting retains caller-supplied response bytes
for compatibility. It must remain outside the untrusted HTTP
boundary.

The current audit detects inconsistencies but does not provide
cryptographic proof against every privileged database mutation.

Production capacity and latency targets require dedicated
load testing.

## Implementation

- Migration 015: authorized transaction reversals.
- Migration 016: exact reversal constraints.
- Migration 017: legacy storage isolation.
- Migration 018: private unified posting engine.
- Migration 019: shared engine adapter integration.

## Validation

The implementation is verified through Rust domain tests,
PostgreSQL integration tests, HTTP integration tests, permission
tests, deterministic lock contention tests, development smoke
tests, and ledger integrity audits.
