# ADR 0002: PostgreSQL for workflows and ledger

Status: accepted for the protocol spike and v0  
Date: 2026-07-30

## Context

Assignments, receipts, write intents, qualifier attribution, and ledger batches
need transactions, uniqueness, auditability, restart recovery, and moderate
work-queue behavior. V0 does not yet have evidence that a broker or distributed
workflow platform is needed.

## Decision

Use PostgreSQL 18 on its current supported minor. Use SQLx 0.9 and forward-only
reviewed SQL migrations. Use transactional job/outbox tables, row locks,
revisions, unique constraints, and leased claims with fencing. Do not introduce
Redis, Kafka, or a separate workflow engine initially.

## Consequences

- Workflow and accounting boundaries can commit atomically where required.
- One operational datastore is enough for the spike.
- Large artifacts and unbounded TIG bodies must remain outside PostgreSQL.
- Queue queries and indexes must be measured; PostgreSQL is not assumed to be
  an unlimited event bus.

## Revisit when

Measured lock contention, queue depth, retention, fan-out, or independent scale
requires a broker or workflow service. Any split must preserve the existing
idempotency and fencing contracts.

