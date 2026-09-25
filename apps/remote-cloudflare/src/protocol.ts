import { createHash } from "node:crypto";
import { Buffer } from "node:buffer";
import { constants, createZstdCompress, zstdDecompressSync } from "node:zlib";
import { Readable } from "node:stream";

export const BLOB_LIMIT = 16 * 1024 * 1024;
export const METADATA_LIMIT = 2 * 1024 * 1024;
export const PACK_LIMIT = 32 * 1024 * 1024;
export const BATCH_LIMIT = 128;
export const SHARDS = 16;
export type Key = { kind: "blob" | "transaction"; id: string };
export type FileEntry = { blob: string; size: number; executable: boolean; kind?: "chunked" | "symlink" | "symlink-directory" };
export type Edit = { parents: string[]; value: FileEntry | null };
export type Transaction = { format: number; nonce: string; message: string; edits: Record<string, Edit>; provenance?: FileEntry };
export type Pin = { name: string; snapshot: { files: Record<string, FileEntry> }; roots: string[] };

export class HttpError extends Error {
  constructor(public status: number, message: string) { super(message); }
}

export function check(condition: unknown, message: string): asserts condition {
  if (!condition) throw new HttpError(400, message);
}

export function objectID(kind: string, bytes: Uint8Array): string {
  return createHash("sha256").update(`kelp/0\0${kind}\0${bytes.length}\0`).update(bytes).digest("hex");
}

export function shard(project: string, kind: string, id: string): number {
  const hash = objectID("placement", Buffer.from(`${project}\0${kind}\0${id}`));
  return Number(BigInt(`0x${hash.slice(0, 16)}`) % BigInt(SHARDS));
}

function record(value: unknown, fields?: string[]): Record<string, unknown> {
  check(value !== null && typeof value === "object" && !Array.isArray(value), "expected an object");
  if (fields) check(Object.keys(value).every(key => fields.includes(key)), "unknown metadata field");
  return value as Record<string, unknown>;
}

export function text(value: unknown): string {
  check(typeof value === "string" && Buffer.from(value).toString("utf8") === value, "expected a valid Unicode string");
  return value;
}

export function name(value: unknown): string {
  const result = text(value);
  check(/^[a-zA-Z0-9_-]{1,128}$/.test(result), "invalid project name or nonce");
  return result;
}

export function hash(value: unknown): string {
  const result = text(value);
  check(/^[0-9a-f]{64}$/.test(result), "invalid object hash");
  return result;
}

export function path(value: unknown): string {
  const result = text(value);
  for (const part of result.split("/")) {
    let bytes = Buffer.from(part);
    if (part.startsWith("\0")) {
      check(/^(?:[0-9a-f]{2})+$/.test(part.slice(1)), "invalid raw filename encoding");
      bytes = Buffer.from(part.slice(1), "hex");
      let validUTF8 = true;
      try { new TextDecoder("utf-8", { fatal: true, ignoreBOM: true }).decode(bytes); } catch { validUTF8 = false; }
      check(!validUTF8, "noncanonical raw filename encoding");
    }
    const lower = bytes.toString("utf8").toLowerCase();
    check(bytes.length && !bytes.includes(0) && !bytes.includes(47) && ![".", "..", ".git", ".kelp"].includes(lower), "invalid project-relative path");
  }
  return result;
}

function hashes(value: unknown): string[] {
  check(Array.isArray(value), "expected hash list");
  return [...new Set(value.map(hash))].sort();
}

export function file(value: unknown): FileEntry {
  const input = record(value, ["blob", "size", "executable", "kind"]);
  const blob = hash(input.blob);
  check(Number.isSafeInteger(input.size) && (input.size as number) >= 0, "invalid file length");
  check(typeof input.executable === "boolean", "invalid executable flag");
  const kind = input.kind ?? "regular";
  check(["regular", "chunked", "symlink", "symlink-directory"].includes(kind as string), "invalid file kind");
  check(kind === "chunked" || (input.size as number) <= BLOB_LIMIT, "unchunked file exceeds 16 MiB");
  check(!String(kind).startsWith("symlink") || !input.executable, "symlink cannot be executable");
  return { blob, size: input.size as number, executable: input.executable, ...(kind === "regular" ? {} : { kind: kind as FileEntry["kind"] }) };
}

export function transaction(value: unknown): Transaction {
  const input = record(value, ["format", "nonce", "message", "edits", "provenance"]);
  check(input.format === 1 || input.format === 2, "unsupported transaction format");
  const nonce = name(input.nonce), message = text(input.message);
  check(message.trim() && Buffer.byteLength(message) <= 16_384, "describe the commit in 1–16384 bytes");
  const edits = Object.fromEntries(Object.entries(record(input.edits)).map(([key, value]) => {
    path(key);
    const edit = record(value, ["parents", "value"]);
    const entry = edit.value === null ? null : file(edit.value);
    check(input.format === 2 || !entry?.kind, "extended file entry requires transaction format 2");
    return [key, { parents: hashes(edit.parents), value: entry }];
  }));
  const result: Transaction = { format: input.format, nonce, message, edits };
  if (input.provenance != null) {
    result.provenance = file(input.provenance);
    check(input.format === 2 && !result.provenance.executable && !result.provenance.kind?.startsWith("symlink"), "invalid provenance");
  }
  return result;
}

export function pin(value: unknown): Pin {
  const input = record(value, ["name", "snapshot", "roots"]);
  const pinName = path(input.name);
  check(Buffer.byteLength(pinName) <= 128, "tag name is too long");
  const snapshot = record(input.snapshot, ["files"]);
  const files = Object.fromEntries(Object.entries(record(snapshot.files)).map(([key, value]) => [path(key), file(value)]));
  for (const key of Object.keys(files)) {
    const parts = key.split("/");
    while (parts.pop() && parts.length) check(!Object.hasOwn(files, parts.join("/")), "file/directory collision");
  }
  return { name: pinName, snapshot: { files }, roots: hashes(input.roots) };
}

// serde preserves struct field order and sorts map keys by UTF-8 bytes. Plain
// JSON.stringify reorders numeric-looking paths, so maps need explicit encoding.
function mapJSON<T>(map: Record<string, T>, encode: (value: T) => string): string {
  return `{${Object.keys(map).sort((a, b) => Buffer.compare(Buffer.from(a), Buffer.from(b)))
    .map(key => `${JSON.stringify(key)}:${encode(map[key])}`).join(",")}}`;
}

export function transactionBytes(tx: Transaction): Buffer {
  const edits = mapJSON(tx.edits, edit => `{"parents":${JSON.stringify(edit.parents)},"value":${JSON.stringify(edit.value)}}`);
  return Buffer.from(`{"format":${tx.format},"nonce":${JSON.stringify(tx.nonce)},"message":${JSON.stringify(tx.message)},"edits":${edits}${tx.provenance ? `,"provenance":${JSON.stringify(tx.provenance)}` : ""}}`);
}

export function snapshotJSON(files: Record<string, FileEntry>): string {
  return `{"files":${mapJSON(files, value => JSON.stringify(value))}}`;
}

export function pinBytes(pin: Pin): Buffer {
  return Buffer.from(`{"name":${JSON.stringify(pin.name)},"snapshot":${snapshotJSON(pin.snapshot.files)},"roots":${JSON.stringify(pin.roots)}}`);
}

export function parseJSON(bytes: Uint8Array): unknown {
  try { return JSON.parse(new TextDecoder("utf-8", { fatal: true, ignoreBOM: true }).decode(bytes)); }
  catch { throw new HttpError(400, "invalid JSON metadata"); }
}

export function keys(value: unknown): Key[] {
  const input = record(value, ["objects"]);
  check(Array.isArray(input.objects) && input.objects.length <= BATCH_LIMIT, "batch exceeds 128 objects");
  const keys = input.objects.map((value): Key => {
    const key = record(value, ["kind", "id"]);
    check(key.kind === "blob" || key.kind === "transaction", "invalid batch object kind");
    return { kind: key.kind, id: hash(key.id) };
  });
  check(new Set(keys.map(key => `${key.kind}/${key.id}`)).size === keys.length, "duplicate batch object");
  return keys;
}

export function syncRequest(value: unknown): { cursors: number[]; paths: string[] } {
  const input = record(value);
  check(Array.isArray(input.cursors), "expected journal cursors");
  const cursors: number[] = input.cursors.length ? input.cursors : Array(SHARDS).fill(0);
  check(cursors.length === SHARDS && cursors.every(cursor => Number.isSafeInteger(cursor) && cursor >= 0), "cursor does not match storage layout");
  check(input.paths === undefined || Array.isArray(input.paths), "invalid path selection");
  return { cursors, paths: (input.paths ?? []).map(path) };
}

export function decodePack(bytes: Uint8Array): { key: Key; bytes: Buffer }[] {
  let raw: Buffer;
  try { raw = zstdDecompressSync(bytes, { maxOutputLength: PACK_LIMIT }); }
  catch { throw new HttpError(400, "invalid or oversized compressed object batch"); }
  check(raw.subarray(0, 4).toString() === "KLP1", "invalid object batch header");
  const objects: { key: Key; bytes: Buffer }[] = [];
  let offset = 4;
  while (offset < raw.length) {
    check(objects.length < BATCH_LIMIT && raw.length - offset >= 69, "invalid object batch framing");
    check(raw[offset] === 0, "transactions must use the publication endpoint");
    const id = hash(new TextDecoder().decode(raw.subarray(offset + 1, offset + 65)));
    const length = new DataView(raw.buffer, raw.byteOffset, raw.byteLength).getUint32(offset + 65);
    offset += 69;
    check(length <= BLOB_LIMIT && offset + length <= raw.length, "invalid object length");
    const bytes = raw.subarray(offset, offset + length);
    check(objectID("blob", bytes) === id, "batch object hash mismatch");
    objects.push({ key: { kind: "blob", id }, bytes });
    offset += length;
  }
  keys({ objects: objects.map(object => object.key) });
  return objects;
}

export function compressPack(chunks: AsyncIterable<Uint8Array>): ReadableStream<Uint8Array> {
  const input = Readable.from(chunks);
  const compressor = createZstdCompress({ params: { [constants.ZSTD_c_compressionLevel]: 1 } });
  input.on("error", error => compressor.destroy(error));
  compressor.on("close", () => input.destroy());
  const output = input.pipe(compressor)[Symbol.asyncIterator]();
  return new ReadableStream({
    async pull(controller) {
      const { value, done } = await output.next();
      if (done) controller.close();
      else controller.enqueue(value);
    },
    async cancel() { await output.return?.(); },
  });
}

export async function body(request: Request, limit: number): Promise<Buffer> {
  const reader = request.body?.getReader();
  if (!reader) return Buffer.alloc(0);
  const chunks: Uint8Array[] = [];
  let size = 0;
  try {
    while (true) {
      const { value, done } = await reader.read();
      if (done) break;
      size += value.length;
      if (size > limit) { await reader.cancel(); throw new HttpError(413, "request exceeds size limit"); }
      chunks.push(value);
    }
  } finally { reader.releaseLock(); }
  return Buffer.concat(chunks, size);
}
