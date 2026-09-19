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
- [ADR 0009: Automated member withdrawal below per-member caps](0009-automated-member-withdrawal-caps.md)
- [ADR 0010: Per-member collateral multiplier](0010-member-collateral-multiplier.md)
- [ADR 0011: The connected wallet is the member account and the payout destination](0011-wallet-is-the-member-account.md)
- [ADR 0012: A round is credited only after its own slashing is settled](0012-settle-after-slashing.md)
- [ADR 0013: A charge does not depend on fault, and its amount is derived](0013-charge-without-fault.md)
