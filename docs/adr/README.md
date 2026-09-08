# Architecture decision records

These records capture v0 choices that would be expensive to reverse. Their
status is authoritative for implementation; the mining rules remain in
[`mining_system.md`](../mining_system.md).

- [ADR 0001: Rust for backend and member agent](0001-rust-for-backend-and-agent.md)
- [ADR 0002: PostgreSQL for workflows and ledger](0002-postgresql-workflow-and-ledger.md)
- [ADR 0003: Filesystem locally and S3 for deployed artifacts](0003-artifact-storage.md)
- [ADR 0004: Isolate TIG credentials and hostile artifact work](0004-process-boundaries.md)
- [ADR 0005: Block-derived challenge-tie draw](0005-challenge-tie-derivation.md)
- [ADR 0006: Allocating the TIG read budget across reader processes](0006-tig-read-budget-allocation.md)
- [ADR 0007: Dedicated browser wallet for testnet API-key provisioning](0007-testnet-browser-wallet.md)
- [ADR 0008: One member balance for earnings, collateral, and withdrawal](0008-single-member-balance.md)
