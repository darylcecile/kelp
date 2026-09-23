# Kelp remote

An Axum HTTP service backed by one SQLite database. Run with `cargo run -p kelp-remote`; set `KELP_TOKEN` first. See the [root README](../../README.md) for Docker deployment.

## v0 API

All `/v0` routes require `Authorization: Bearer <KELP_TOKEN>`. `/healthz` is public. Metadata is JSON, object transfers are raw bytes. Errors have `code` and `message` fields; malformed request bodies may be rejected directly by the HTTP framework.

| Method | Path | Purpose |
| --- | --- | --- |
| `GET` | `/healthz` | Liveness |
| `PUT` | `/v0/projects/{project}` | Create a project idempotently |
| `GET` | `/v0/projects/{project}` | Get project/protocol information |
| `PUT` | `/v0/projects/{project}/objects/{kind}/{id}` | Upload a blob or snapshot |
| `GET`, `HEAD` | `/v0/projects/{project}/objects/{kind}/{id}` | Retrieve/check a blob, snapshot, or published revision |
| `GET` | `/v0/projects/{project}/changes` | List current changes and revision heads |
| `GET` | `/v0/projects/{project}/changes/{change}` | Read a change's heads |
| `POST` | `/v0/projects/{project}/changes/{change}/publications` | Publish an exact revision |

Names contain ASCII letters, digits, hyphens, or underscores. Object IDs are 64 lowercase hexadecimal characters. The shared `kelp-core` types define request/response fields.

## Object identity

```text
SHA-256("kelp/0\0" + kind + "\0" + decimal_byte_length + "\0" + bytes)
```

For snapshots and revisions, `bytes` is compact JSON in the field order of the shared Rust structs, with path maps sorted lexicographically. This encoding is a prototype contract, not generic JSON canonicalization. Other languages can implement the same HTTP protocol and encoding without running the Rust service.

Upload file blobs before snapshots. A snapshot is accepted only if all its referenced blobs exist in that project and their sizes match. Publication requires complete base/result snapshots and a valid predecessor in the same change. Metadata publication and the idempotency receipt commit together.

```json
{
  "request_id": "a-client-generated-unique-id",
  "revision": {
    "project": "demo",
    "change": "a-client-generated-change-id",
    "predecessor": null,
    "base_snapshot": "<64-character snapshot ID>",
    "result_snapshot": "<64-character snapshot ID>",
    "message": "Fix checkout timeout"
  }
}
```

Reusing a request ID with the same payload returns the original receipt. Reusing it with different contents returns `409`. Updating an older predecessor preserves competing heads; replaying an already published revision never resurrects an obsolete head.

The service serializes SQLite work on blocking threads, keeping it off the async HTTP executor. v0 lists are not paginated, uploads are bounded to 16 MiB, and metadata is bounded to 2 MiB. This implementation does not claim multi-node replication or fine-grained user permissions.
