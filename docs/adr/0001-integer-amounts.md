# ADR-001: Use of 64-bit signed integers for monetary amounts

## Status

Accepted

## Context

The West African CFA Franc (XOF) has no fractional subdivision. Using floating-point numbers (`f32` or `f64`) introduces unacceptable rounding errors for financial systems.

## Decision

All monetary amounts will be represented internally by an `Amount` type encapsulating a 64-bit signed integer (`i64`), expressed in whole XOF. Arithmetic operations will use checked methods (e.g., `checked_add`) to prevent silent overflows.

## Consequences

- **Positive**: Absolute precision, no rounding errors, overflows are explicitly detected and rejected.
- **Negative**: Requires a custom `Amount` wrapper instead of native primitives.
