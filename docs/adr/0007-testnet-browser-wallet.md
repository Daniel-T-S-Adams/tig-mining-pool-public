# ADR 0007: Dedicated browser wallet for testnet API-key provisioning

Status: accepted for testnet development  
Date: 2026-09-04

## Context

The original credential boundary required the TIG account signing key to be
used offline for API-key provisioning. That is a suitable production posture,
but TIG testnet provides an official browser flow that connects a wallet,
requests the canonical ownership signature, exchanges it for an API key, and
allows the operator to retrieve that key from the authenticated session.

The Slice 1 operator has selected a new, dedicated MetaMask testnet wallet and
successfully used this flow. The wallet has no production authority or mainnet
funds. Treating that observed procedure as though it were offline would leave
the architecture and the repeatable operator instructions inconsistent.

## Decision

For testnet development only, a dedicated operator-controlled browser wallet
may hold the TIG account signing key and perform manual API-key issuance or
rotation through the official TIG testnet UI. The operator follows the exact
origin, message, and result checks in `tig_integration.md` section 4.

The browser wallet is part of the operator workstation, not any pool process.
Its recovery phrase, private key, password, and signed ownership proof never
enter the repository, workspace, runtime hosts, CI, logs, environment variables
or agent sessions. Only the resulting API key crosses into the untracked local
secret file and, later, the gateway's scoped deployment secret.

The operator UI is not a runtime dependency and is not added to the pool's
machine-readable API pins. It is revalidated against the pinned canonical
`POST /request-api-key` contract on every manual use.

This exception does not apply to production or mainnet. Production provisioning
requires a separate custody decision with an offline, hardware-backed or
managed signing key, multi-person recovery, and a tested rotation runbook.

## Consequences

- Testnet provisioning matches the supported TIG operator experience and is
  straightforward to repeat.
- The testnet identity now trusts the operator workstation, browser extension,
  and official TIG UI during provisioning. Compromise can expose testnet
  authority, so the wallet cannot hold production authority or mainnet funds.
- API-key issuance, replacement, and revocation remain audited operator
  actions. Replacing the local secret does not prove that an older upstream key
  was invalidated.
- Pool runtime isolation is unchanged: only `tig-gateway` receives the API key,
  and no runtime receives the account signing key.

## Revisit when

- TIG offers a supported hardware-wallet, delegated, scoped, or explicit
  revocation flow.
- A testnet wallet would receive non-trivial value or authority.
- Production or mainnet credential provisioning is designed.
