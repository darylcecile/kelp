# 5. Native protocol and synchronization contract

[Overview](README.md) · [Data model](03-data-model.md) · [Delivery plan](06-delivery-and-validation.md)

This is a proposed v1 behavioral specification. It is detailed enough to guide a prototype; canonical encodings, test vectors, and transport limits must be frozen before claiming interoperability. JSON below illustrates messages rather than replacing the canonical object format.

## Design rules

1. Separate discovering state, transferring bytes, publishing proposals, and accepting work.
2. Allow any authorized provider to serve an immutable object.
3. Never require a particular gateway process to remember a client's session.
4. Bound and paginate discovery; opening a workspace must not enumerate every change or branch.
5. Pin reads to immutable IDs. “Latest” is a metadata lookup, not a property of object bytes.
6. Expose partial availability, index lag, and durability level in responses.
7. Persist write outcomes before replying, so retries can resolve ambiguity.

HTTPS is the reference network transport. File bundles carry the same objects and events for offline exchange. Alternative transports must preserve these semantics.

## Discovery and identity

```http
GET /v1/projects/P_SHOP
```

```json
{
  "protocol": "kelp/1",
  "project": "P_SHOP",
  "identity_head": "I12",
  "authority_epoch": 4,
  "capabilities": ["partial-read", "batch-objects", "native-bundle"],
  "hash_algorithms": ["sha256"],
  "channels_url": "/v1/projects/P_SHOP/channels",
  "durability_profiles": ["regional"],
  "limits": {"batch_object_ids": 4096, "page_items": 1000}
}
```

Limits shown are provisional defaults and advertised by the server. Clients verify identity updates from the trusted genesis/previous identity. Changing providers does not change the project ID.

Capability negotiation must fail clearly when a required object/merge profile is unsupported. Unknown optional response fields can be ignored; unknown required semantics cannot.

## Read a coherent project view

### Step 1: resolve a channel once

```http
GET /v1/projects/P_SHOP/channels/main?consistency=authoritative
```

```json
{
  "entry": "E1848",
  "sequence": 1848,
  "snapshot": "S92BC",
  "authority_epoch": 4,
  "freshness": "authoritative",
  "signed_entry": "ENVELOPE_1848"
}
```

A cached response is allowed only when requested, and includes when the authority was last checked. A response from the authoritative service is current at a defined read point, not guaranteed to remain latest after the response.

### Step 2: request the selected paths at that snapshot

```http
POST /v1/projects/P_SHOP/read-plans
```

```json
{
  "snapshot": "S92BC",
  "paths": ["services/checkout", "libs/money"],
  "have_roots": ["H_LOCAL_CACHED_SUBTREE"],
  "include_history": false
}
```

The paginated response provides map pages/proofs, file manifests, object descriptors, and authorized provider URLs. It identifies the snapshot and selection on every page. Continuation tokens bind the selection and root; they never silently advance to newer channel state.

Clients compare hashes and descend only into needed subtrees. `have_roots` is an optimization, not evidence of possession or authorization. If a client falsely claims to have data, it must fetch it later or report an incomplete local view.

### Step 3: obtain bytes

```http
POST /v1/projects/P_SHOP/objects:batch-get
```

```json
{
  "root": "S92BC",
  "objects": ["sha256:OBJECT_A", "sha256:OBJECT_B"]
}
```

Responses stream independent object frames with type, ID, length, and bytes, or return authorized URLs for larger content. Completed objects survive an interrupted request. Large-object delivery can resume by verified chunk; an incomplete chunk is not installed as valid content.

Provider hints and caches are replaceable. Hashes verify bytes; authorization verifies whether the requester may receive them. A missing object from one provider is retried at another advertised provider, within a bounded retry policy.

## One publish command, local capture followed by upload

The CLI combines these steps into one operation:

1. Capture the current tracked-file state, or the explicit checkpoint/revision selected by the caller.
2. Allocate the change identity on first publication and record the exact revision and request ID durably on the client.
3. Upload its required objects, then publish that revision through the endpoint below.

Later edits do not alter an in-flight revision. A failed or ambiguous request is resolved using the same request ID and revision before a later invocation publishes newer edits. If nothing changed after a successful publication, the CLI reports “already published.”

Automatic checkpoints are local operation records, not uploads or review revisions. They use the same snapshot/content encodings without entering the published revision ancestry. Clients can record named checkpoints and prepare offline bundles without a server-issued identity or revision number.

### Upload preparation

```http
POST /v1/projects/P_SHOP/uploads
```

```json
{
  "roots": ["R8A31"],
  "durability": "regional"
}
```

The host returns an upload ID and requests the missing graph incrementally. The client uploads typed objects in batches. The host verifies hashes, referential structure, authorized reuse, and the revision's declared edit set.

When the required closure is complete and replicated, the host issues a signed receipt for the roots. The receipt includes an expiry time for its temporary retention pin. It is not a permanent publication acknowledgment.

### Publication

```http
POST /v1/projects/P_SHOP/changes/C7K2/publications
Idempotency-Key: PUB_519
```

```json
{
  "revision": "R8A31",
  "observed_heads": ["R_PREVIOUS"],
  "upload_receipt": "UP_91",
  "signature": "SIG_MAYA"
}
```

```json
{
  "publication": "PUB_519",
  "state": "published",
  "heads": ["R8A31"],
  "diverged": false,
  "durability": "regional"
}
```

For the first publication, `observed_heads` is empty and the uploaded closure includes the signed change-creation record. The endpoint creates the change's metadata record and publishes its first revision atomically; no separate remote create-change request is required.

If an unobserved competing revision exists, the response can still be successful with `diverged: true` and both heads. Removing an observed head requires a valid predecessor relationship. Arbitrary stale-head replacement is not supported.

For a stack, a publication root lists its dependency-closed exact revision set. Objects are durable first, and the bundle becomes discoverable through one publication record. Per-change indexes may update asynchronously; readers following that record can retrieve the complete stack immediately.

## Landing is an asynchronous request

```http
POST /v1/projects/P_SHOP/channels/main/landings
Idempotency-Key: LAND_83
```

```json
{
  "revisions": ["R8A31"],
  "observed_entry": "E1847",
  "on_target_advance": "revalidate"
}
```

The server returns `202 Accepted` with a request ID, not a claim that code has landed.

```text
queued → preparing → checking → ready → accepted
               │          │        │
               │          │        └── target moved → preparing
               └──────────┴── blocked / rejected
```

`GET .../landings/LAND_83` returns the recorded state, candidate snapshot, expected channel entry, check evidence, and terminal receipt when available.

`on_target_advance: reject` instead requires the client to explicitly resubmit after a stale target. Automatic revalidation never changes the selected revision IDs. A source conflict blocks the request and returns a saved draft result for resolution.

### The atomic decision

Equivalent transactional pseudocode:

```text
if a final result for LAND_83 exists:
    return that result

require channel.entry == candidate.expected_entry
require channel.policy_version == candidate.policy_version
require channel authorization/admission state permits the landing
require every required check binds this exact candidate
require candidate closure has a live durable pin

write channel.entry = candidate.new_entry
write landing[LAND_83] = accepted(candidate.new_entry)
write publication/retention roots
commit as one transaction
```

The exact durable objects are stored before this transaction. Its purpose is to decide visibility, not perform a distributed write to every file.

The service durably retains terminal landing outcomes with channel history. Reusing an idempotency key with a different request body is an error. This provides one recorded effect under retries, not magical exactly-once delivery of network packets.

## Reviews and checks

Review events name an exact revision and a stable change ID. Comments may additionally name a path, file ID, and byte/line context. A later revision can show old comments as outdated; reattachment is a UI suggestion, not rewriting their original target.

Check attestations contain:

- Candidate snapshot and expected channel entry.
- Ordered revision list and merge profile.
- Policy version and check definition/version.
- External build-input identifiers where applicable.
- Result and signer identity.

The channel authority validates evidence under its policy. An imported peer's “approved” event is visible discussion history, not automatically a trusted local approval.

Approval withdrawal and permission changes affecting pending requests use the ordered admission mechanism described in [distribution](04-distribution.md). A stale cached review list is insufficient for landing.

## Synchronize with another client or host

Synchronization scopes are explicit: channel entries, selected changes, review events, or a full mirror.

1. Exchange project identity and supported profiles.
2. Request head summaries for subscribed scopes, using opaque continuation cursors.
3. Compare immutable frontier IDs and walk only missing history.
4. Batch-fetch missing objects and verify signatures/authorization.
5. Install a scope update only when its required closure is available or explicitly marked lazy with provider coverage.

Delivery can repeat or arrive out of order. Immutable IDs make duplicates harmless. Predecessors are fetched before applying their effect to derived head state. A receiver never treats packet arrival order or wall-clock time as the accepted order of a channel.

Cursor expiration returns `CURSOR_EXPIRED` and triggers bounded rediscovery of the selected scope, not a mandatory full-project clone.

Full native bundles contain an inventory, typed objects, signed identity history, authority entries, and explicit completeness claims. A delta bundle also lists prerequisite roots. `kelp bundle verify` checks those claims and reports missing prerequisites before import.

## History queries and partial answers

```http
GET /v1/projects/P_SHOP/history?path=libs/money&at=E1848&limit=100
```

The response includes `indexed_through`, requested `at`, pagination, and `complete_for_request`. A caller can require exactness, allow a labeled stale answer, or ask the service to wait for an index.

The server must not silently turn “not in this cache” into “never existed.” Exact queries may be slower and fetch more data. Broad blame, search, or full ancestry traversal may scale with history or project size.

## Error contract

| Code | Meaning | Next action |
| --- | --- | --- |
| `TARGET_ADVANCED` | Candidate was based on an older channel entry | Revalidate or explicitly resubmit |
| `REVISION_DIVERGED` | More than one revision head exists | Choose an exact revision or reconcile |
| `DEPENDENCY_MISSING` | Required revision is not accepted/in the requested set | Add the named prerequisite or revise requirements |
| `CONFLICT_UNRESOLVED` | Candidate contains a conflict | Open the saved conflict result |
| `OBJECT_UNAVAILABLE` | Required bytes are not currently retrievable | Try a provider or restore a missing replica |
| `NOT_CACHED_OFFLINE` | Local operation needs absent data | Connect or choose a pinned scope |
| `AUTHORITY_UNAVAILABLE` | No quorum for an authoritative decision | Keep local work; retry later |
| `RECEIPT_EXPIRED` | Temporary upload pin expired | Revalidate/re-pin before publication |
| `UNSUPPORTED_PROFILE` | Required format/algorithm is unsupported | Upgrade or use an agreed export |
| `IDEMPOTENCY_CONFLICT` | A request ID was reused with different inputs | Use a new ID for the new request |

Unauthorized or nonexistent private objects have indistinguishable external responses where needed to avoid disclosure. Structured errors include a request ID and whether any durable publication occurred.

## What must be specified before protocol freeze?

- Canonical encoding and hash/signature test vectors.
- Path-map construction, proof format, and chunking profile.
- Deterministic merge test vectors, including repeated text and structural conflicts.
- HTTP framing, compression negotiation, byte limits, and cursor lifetime behavior.
- Identity key rotation, authority epoch handoff, and recovery tests.
- Retention and completeness rules for full versus delta bundles.

These are bounded engineering specifications still to write, not capabilities assumed to be solved by choosing a database.
