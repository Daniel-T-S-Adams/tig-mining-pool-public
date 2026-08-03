# ADR 0003: Filesystem locally and S3 for deployed artifacts

Status: accepted for the protocol spike and v0  
Date: 2026-07-30

## Context

A package can be tens of GiB, must survive a pool-process restart after its
receipt, and is temporary after the benchmark becomes active or terminal. The
member protocol must not depend on one storage product.

## Decision

Define one streaming `ArtifactStore` port. Use a filesystem implementation for
local development and CI. Use AWS S3 Standard, with separate quarantine and
accepted locations and separate environments, for deployed testnet and
production. Store protocol SHA-256 independently of provider checksums and do
not treat a multipart ETag as a content hash.

Publication is an ordered object-store/database saga. Only a verified immutable
accepted object can receive a database pointer and durable receipt. Only the
Artifact Worker deletes objects from controller-issued deletion jobs.

## Consequences

- Testnet exercises the same remote-store behavior as production.
- PostgreSQL remains compact.
- Cross-store atomicity is implemented through deterministic keys, retries,
  reference checks, and orphan cleanup rather than assumed.
- The testnet needs S3 credentials and incurs temporary storage and transfer
  cost.

## Revisit when

The spike shows S3 transfer, proof-read latency, cost, or regional availability
is unacceptable. Another backend must pass the same publication, integrity,
restart, and deletion contract before replacing it.

