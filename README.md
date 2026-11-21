# 🌳 Kauri Ledger

An open-source, reliable, and auditable double-entry accounting engine, specifically designed for the realities of West Africa (UEMOA, XOF, Mobile Money, unstable networks).

> **Status**: Phase 0 (Scoping). Foundation laid.  
> **Note**: The API boilerplate was efficiently generated using [cargo-axum-forge](https://crates.io/crates/cargo-axum-forge).

## Why?
Most applications handle money using a simple "balance" column in a database. This inevitably leads to duplicate charges, half-failed operations, and disputes that are impossible to audit. Kauri Ledger enforces double-entry bookkeeping, immutability, and idempotence to guarantee that **every Franc is traceable**.

## Core Principles
- **Double-Entry**: Every transaction is balanced (Total Debits = Total Credits).
- **Immutability**: Nothing is ever deleted. Errors are corrected via reversing entries.
- **Idempotence**: A request sent twice produces exactly one ledger entry.
- **XOF Native**: Amounts are strictly integers (no floats, no cents).

## Project Structure (Cargo Workspace)
- `kauri-ledger-core`: Pure domain logic (types, rules, invariants). No I/O.
- `kauri-ledger-store`: PostgreSQL persistence layer using `sqlx`.
- `kauri-ledger-api`: HTTP REST API built with `axum` and `utoipa` (OpenAPI).
- `kauri-ledger-cli`: Command-line tools for migrations and integrity checks.

## Next Steps
See the [Architecture Decision Records](docs/adr/) and the [Roadmap](docs/guides/roadmap.md).
