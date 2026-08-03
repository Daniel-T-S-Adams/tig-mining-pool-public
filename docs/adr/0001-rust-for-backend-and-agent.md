# ADR 0001: Rust for backend and member agent

Status: accepted for the protocol spike and v0  
Date: 2026-07-30

## Context

The member and pool must reproduce pinned TIG serialization, quality, Merkle,
and proof behavior while processing large untrusted artifacts. Maintaining the
same domain types across agent, ingestion, proof construction, and TIG payloads
reduces cross-language drift.

## Decision

Use one stable Rust 2024 Cargo workspace. Use Tokio and Axum 0.8 for HTTP,
Serde for typed serialization, SQLx 0.9 for PostgreSQL, and small shared domain
crates that do not depend on service infrastructure. Pin the compiler and all
resolved crates in the repository.

## Consequences

- The member and backend share lossless integer, digest, and protocol types.
- Native binaries fit member CPU/GPU hosts without a second application
  runtime.
- Compile time and Rust expertise are accepted costs.
- Web UI code may use a web-native language later; that does not change this
  mining-system decision.

## Revisit when

Pinned TIG components cannot be safely invoked or reproduced from Rust, or an
independently deployable component has a measured reason to use another
language without duplicating protocol semantics.

