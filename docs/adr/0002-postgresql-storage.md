# ADR-002: PostgreSQL as the primary storage engine

## Status

Accepted

## Context

A ledger requires strict ACID guarantees. The system must handle concurrency (row-level locking) and support complex queries for auditing.

## Decision

We will use PostgreSQL as the single relational database. Interaction will be handled via the Rust `sqlx` crate, which allows compile-time verification of SQL queries.

## Consequences

- **Positive**: Proven maturity, native integrity constraints (UNIQUE, CHECK), excellent Rust ecosystem.
- **Negative**: Less "exotic" than specialized ledger databases, but fully sufficient for MVP performance targets.
