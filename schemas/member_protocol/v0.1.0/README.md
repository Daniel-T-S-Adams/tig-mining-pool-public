# Member protocol schemas 0.1.0

These JSON Schemas are the machine-readable companion to
[`docs/member_protocol.md`](../../../docs/member_protocol.md). They use JSON
Schema Draft 2020-12.

The version directory is immutable after implementation begins. A semantic or
wire-format change gets a new directory and explicit negotiation support.

## Files

- `common.schema.json`: identifiers, digests, compute descriptions, states,
  package limits, and the common error response.
- `api.schema.json`: JSON request and response definitions for control and
  upload operations.
- `package-manifest.schema.json`: `manifest.json` inside proof-material v1.
- `output-record.schema.json`: each canonical JSON line in `outputs.ndjson`.

## Endpoint schema map

JSON Pointer fragments below are normative. Every JSON request and response is
validated against the selected definition.

| Operation | Request schema | Success response schema |
|---|---|---|
| `GET /member/v0/protocol` | empty body | `api.schema.json#/$defs/ProtocolInfoResponse` |
| `POST /member/v0/enroll` | `api.schema.json#/$defs/EnrollRequest` | `api.schema.json#/$defs/EnrollResponse` |
| `POST /member/v0/workers/{worker_id}/credentials/rotate` | `api.schema.json#/$defs/RotateCredentialRequest` | `api.schema.json#/$defs/RotateCredentialResponse` |
| `POST /member/v0/workers/{worker_id}/slots` | `api.schema.json#/$defs/RegisterSlotRequest` | `api.schema.json#/$defs/RegisterSlotResponse` |
| `POST /member/v0/slots/{slot_id}/qualification/task` | `common.schema.json#/$defs/EmptyRequest` | `api.schema.json#/$defs/QualificationTaskResponse` |
| `POST /member/v0/slots/{slot_id}/qualification/result` | `api.schema.json#/$defs/QualificationResultRequest` | `api.schema.json#/$defs/QualificationStatusResponse` |
| `POST /member/v0/slots/{slot_id}/qualification/status` | `common.schema.json#/$defs/EmptyRequest` | `api.schema.json#/$defs/QualificationStatusResponse` |
| `POST /member/v0/capacity-offers` | `api.schema.json#/$defs/CapacityOfferRequest` | `api.schema.json#/$defs/CapacityOfferStatusResponse` |
| `POST /member/v0/capacity-offers/{offer_id}/status` | `common.schema.json#/$defs/EmptyRequest` | `api.schema.json#/$defs/CapacityOfferStatusResponse` |
| `POST /member/v0/heartbeats` | `api.schema.json#/$defs/HeartbeatRequest` | `api.schema.json#/$defs/HeartbeatResponse` |
| `POST /member/v0/assignments/{assignment_id}/ack` | `api.schema.json#/$defs/AssignmentAckRequest` | `api.schema.json#/$defs/AssignmentStateResponse` |
| `POST /member/v0/assignments/{assignment_id}/events` | `api.schema.json#/$defs/AssignmentEventRequest` | `api.schema.json#/$defs/AssignmentEventResponse` |
| `POST /member/v0/assignments/{assignment_id}/cancel` | `api.schema.json#/$defs/CancelAssignmentRequest` | `api.schema.json#/$defs/AssignmentStateResponse` |
| `POST /member/v0/assignments/{assignment_id}/status` | `common.schema.json#/$defs/EmptyRequest` | `api.schema.json#/$defs/AssignmentStateResponse` |
| `POST /member/v0/assignments/{assignment_id}/uploads` | `api.schema.json#/$defs/BeginUploadRequest` | `api.schema.json#/$defs/BeginUploadResponse` |
| `PUT /member/v0/uploads/{upload_id}` | raw bytes plus the headers below | `api.schema.json#/$defs/UploadChunkResponse` |
| `POST /member/v0/uploads/{upload_id}/finalize` | `api.schema.json#/$defs/FinalizeUploadRequest` | `api.schema.json#/$defs/UploadStatusResponse` |
| `POST /member/v0/uploads/{upload_id}/status` | `common.schema.json#/$defs/EmptyRequest` | `api.schema.json#/$defs/UploadStatusResponse` |

Every non-success JSON response uses
`common.schema.json#/$defs/ErrorResponse`. It includes `request_id` for a
decoded signed request, `enrollment_request_id` for a decoded enrollment
request, and may omit both for public discovery or an undecodable request.

## Raw upload request

The upload `PUT` body is one contiguous package byte range and is not JSON.
In addition to the signed-request headers in the protocol document, it has:

```text
Content-Type: application/octet-stream
Content-Length: <1 through negotiated chunk size>
Upload-Offset: <0 through declared compressed package size - 1>
Upload-Chunk-SHA256: <64 lowercase hexadecimal characters>
```

The final chunk may be smaller than the negotiated chunk size. An empty chunk
is invalid. The server's `committed_offset` is the only resume authority.

## Validation beyond JSON Schema

JSON Schema cannot express several cross-field and binary invariants. Both
sides must also enforce the protocol document, including:

- `end_exclusive == num_nonces` because nonce range starts at zero;
- confirmed precommit `num_nonces` equals the assignment nonce count;
- Merkle tree capacity is the next power of two at least as large as
  `num_nonces`;
- assignment digest is SHA-256 over RFC 8785 canonical
  `assignment_identity`;
- assignment slot generation and compute specification equal the generation
  covered by the successful qualification-spec digest;
- confirmed precommit `compute_type` equals assignment `compute.compute_type`,
  and the runtime platform architecture equals `compute.cpu_arch`;
- runtime image manifest digests are sorted by ascending UTF-8 byte order
  before calculating the qualification-spec digest;
- a `QUEUED` offer has `precommit_state = NOT_STARTED`, remains live only
  through its renewable lease, and consumes no TIG or collateral reservation;
- a queued promotion is authorized only when the worker echoes the exact
  server-issued `ready_check_id` for the same `offer_id` in its next signed
  heartbeat before lease expiry;
- `workflow_expiry_block == block_started + 120`,
  `proof_reserve_blocks == 10`, and `package_due_before_block ==
  workflow_expiry_block - proof_reserve_blocks` for the spike;
- all package IDs and identity fields match the assignment;
- quality bytes equal `4 * num_nonces`;
- leaf-hash bytes equal `32 * num_nonces`;
- every package file `record_count` equals `num_nonces`;
- output nonces are exactly `0..num_nonces - 1` in order;
- the package is exactly one Zstandard frame with declared content size,
  checksum enabled, no dictionary/skippable/trailing frame or data, and a
  decompression window no greater than 64 MiB;
- declared file sizes, SHA-256 values, archive sizes, and calculated Merkle root
  match; and
- assignment-specific limits are internally ordered and no larger than the
  protocol ceilings.

Numeric JSON integers are limited to the exact IEEE-754 integer range and must
still be parsed losslessly. Full-width unsigned-64 runtime signatures and fuel
values use canonical decimal strings and require a semantic
`<= 18446744073709551615` check. Implementations must never round either form.
