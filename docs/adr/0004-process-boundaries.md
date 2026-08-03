# ADR 0004: Isolate TIG credentials and hostile artifact work

Status: accepted for the protocol spike and v0  
Date: 2026-07-30

## Context

The public API parses member requests, package processing handles large hostile
data, the controller makes financially relevant decisions, and TIG writes use
a privileged API key. Putting all four roles in one process would make a member
endpoint or decompression flaw a direct credential compromise and allow bulk
work to delay protocol deadlines.

## Decision

Deploy Pool API, controller, Artifact Worker, and TIG Gateway as separate
process identities. Only the gateway receives the TIG API key. Only the
Artifact Worker parses packages and builds proof bytes. The controller owns
decisions, lifecycle, proof deadlines, attribution, and accounting. Durable
handoffs use PostgreSQL jobs and intents with distinct database roles.

The controller's internal modules may share its process, and the Artifact
Worker's bounded job classes may share its process. These module boundaries are
Rust ports and durable row contracts so they can split later.

## Consequences

- Internet-facing and hostile-data code cannot directly use TIG credentials.
- Heavy proof work cannot block the controller runtime.
- The smallest deployment has more processes and per-role credentials than a
  monolith.
- Database grants and network policy are part of the boundary, not optional
  hardening.

## Revisit when

A stronger hardware-backed signing service is introduced, or measured load
requires separate ingestion, proof, attribution, or accounting processes.
Components may split further; the public API and gateway must not merge.

