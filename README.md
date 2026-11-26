# 🌳 Kauri Ledger

An open-source, reliable, and auditable double-entry accounting engine, specifically designed for the realities of West Africa (UEMOA, XOF, Mobile Money, unstable networks).

> **Status**: Development — multi-tenant accounting, integrity auditing,
> tenant-authorized HTTP operations, and exact transaction reversals
> implemented. Production readiness remains under evaluation. 
> **Note**: The API boilerplate was efficiently generated using [cargo-axum-forge](https://crates.io/crates/cargo-axum-forge).

## Why?
Most applications handle money using a simple "balance" column in a database. This inevitably leads to duplicate charges, half-failed operations, and disputes that are impossible to audit. Kauri Ledger enforces double-entry bookkeeping, immutability, and idempotence to guarantee that **every Franc is traceable**.

## Core Principles
- **Double-Entry**: Every transaction is balanced (Total Debits = Total Credits).
- **Immutability**: Nothing is ever deleted. Errors are corrected via reversing entries.
- **Idempotence**: Retrying the same valid request does not create an
  additional transaction or repeat its accounting effects.
- **XOF Native**: Amounts are strictly integers (no floats, no cents).

## Project Structure (Cargo Workspace)
- `kauri-ledger-core`: Pure domain logic (types, rules, invariants). No I/O.
- `kauri-ledger-store`: PostgreSQL persistence layer using `sqlx`.
- `kauri-ledger-api`: HTTP REST API built with `axum` and `utoipa` (OpenAPI).
- `kauri-ledger-cli`: Command-line tools for migrations and integrity checks.


## Implemented Capabilities

- Integer-based XOF double-entry accounting.
- Multi-tenant ledger isolation.
- Atomic transaction posting and balance updates.
- Immutable journal entries and transaction seals.
- Exact compensating transactions.
- Database-enforced idempotency.
- Scoped API keys and audited key lifecycle.
- Accounting integrity verification through the CLI.
- PostgreSQL development environment with Docker Compose.
- HTTP API documentation through OpenAPI and Swagger UI.

## HTTP API

| Method | Endpoint | Scope |
|--------|----------|-------|
| GET | `/health` | Public |
| POST | `/v1/ledgers/{ledger_id}/transactions` | write |
| POST | `/v1/ledgers/{ledger_id}/transactions/{id}/reverse` | write |
| GET | `/v1/ledgers/{ledger_id}/transactions/{id}` | read |
| GET | `/v1/ledgers/{ledger_id}/accounts/{code}/balance` | read |

## Development Validation

```bash
cargo fmt --all -- --check
cargo check --locked --all-targets --all-features
cargo test --locked --all-targets --all-features
cargo clippy --locked --all-targets --all-features -- -D warnings
```

Integration tests require a running Docker-compatible container
environment.

## Technical Documentation

- [Unified transaction engine](docs/adr/0004-unified-transaction-engine.md)
- [Transaction reversals](docs/transaction-reversals.md)
- [Architecture decision records](docs/adr/)


## Next Steps
See the [Architecture Decision Records](docs/adr/).
