import type { Env } from "./index";
import { Buffer } from "node:buffer";
import {
  BATCH_LIMIT, BLOB_LIMIT, FileEntry, HttpError, Key, METADATA_LIMIT, PACK_LIMIT, Pin, SHARDS, Transaction,
  check, compressPack, file, objectID, parseJSON, pin, pinBytes, shard, snapshotJSON, transaction, transactionBytes,
} from "./protocol";

async function map<T, R>(values: T[], fn: (value: T) => Promise<R>): Promise<R[]> {
  const result: R[] = [];
  for (let i = 0; i < values.length; i += 8) result.push(...await Promise.all(values.slice(i, i + 8).map(fn)));
  return result;
}

export class Store {
  constructor(private env: Env, private project: string) {}

  journal(index: number) {
    return this.env.JOURNALS.get(this.env.JOURNALS.idFromName(`${this.project}/${index}`));
  }

  key(kind: string, id: string): string { return `${this.project}/${kind}/${id}`; }

  async projectInfo(create: boolean) {
    const key = this.key("project", "kelp-1");
    if (create) await this.env.OBJECTS.put(key, "kelp/1");
    else if (!await this.env.OBJECTS.head(key)) throw new HttpError(404, "project does not exist");
    return {
      project: this.project, protocol: "kelp/1", storage_nodes: SHARDS, replicas: 1,
      layout: objectID("layout", Buffer.from(`cloudflare/r2-journals/v1/${SHARDS}`)),
    };
  }

  async info(keys: Key[]) {
    const sizes = new Map<string, number | null>();
    const groups = new Map<number, string[]>();
    for (const key of keys.filter(key => key.kind === "transaction")) {
      const owner = shard(this.project, key.kind, key.id);
      groups.set(owner, [...(groups.get(owner) ?? []), key.id]);
    }
    await map([...groups], async ([owner, ids]) => {
      const found = await this.journal(owner).info(ids);
      ids.forEach((id, i) => sizes.set(id, found[i]));
    });
    return map(keys, async key => {
      const size = key.kind === "transaction" ? sizes.get(key.id) ?? null
        : (await this.env.OBJECTS.head(this.key(key.kind, key.id)))?.size ?? null;
      return { object: key, size, copies: size === null ? 0 : 1 };
    });
  }

  async get(kind: string, id: string): Promise<Buffer> {
    if (kind === "transaction" && (await this.info([{ kind, id }]))[0].size === null) {
      throw new HttpError(404, `transaction ${id} has not been published`);
    }
    const object = await this.env.OBJECTS.get(this.key(kind, id));
    if (!object) throw new HttpError(404, `missing ${kind} ${id}`);
    const bytes = Buffer.from(await object.arrayBuffer());
    if (objectID(kind, bytes) !== id) throw new HttpError(503, "stored object hash mismatch");
    return bytes;
  }

  async upload(id: string, bytes: Uint8Array): Promise<void> {
    check(bytes.length <= BLOB_LIMIT && objectID("blob", bytes) === id, "invalid blob hash or size");
    await this.env.OBJECTS.put(this.key("blob", id), bytes);
  }

  async uploadPack(objects: { key: Key; bytes: Buffer }[]): Promise<void> {
    // decodePack validates the entire batch before any writes begin.
    await map(objects, async object => this.upload(object.key.id, object.bytes));
  }

  async download(keys: Key[]): Promise<ReadableStream<Uint8Array>> {
    const infos = await this.info(keys);
    let size = 4;
    for (const info of infos) {
      if (info.size === null) throw new HttpError(404, "requested object is missing");
      size += 69 + info.size;
    }
    check(size <= PACK_LIMIT, "object batch exceeds 32 MiB");
    return compressPack(this.packObjects(keys));
  }

  private async *packObjects(keys: Key[]): AsyncGenerator<Uint8Array> {
    yield Buffer.from("KLP1");
    for (const key of keys) {
      const bytes = await this.get(key.kind, key.id);
      const header = Buffer.alloc(69);
      header[0] = key.kind === "blob" ? 0 : 1;
      header.write(key.id, 1, 64, "ascii");
      header.writeUInt32BE(bytes.length, 65);
      yield header;
      yield bytes;
    }
  }

  async verifyFiles(entries: FileEntry[]): Promise<void> {
    const seen = new Set<string>();
    while (entries.length) {
      const group = entries.splice(-BATCH_LIMIT).filter(entry => {
        const key = JSON.stringify(entry);
        if (seen.has(key)) return false;
        seen.add(key);
        return true;
      });
      const unique = [...new Set(group.map(entry => entry.blob))];
      const infos = await this.info(unique.map(id => ({ kind: "blob", id })));
      const sizes = new Map(infos.map(info => [info.object.id, info.size]));
      for (const entry of group) {
        if (entry.kind !== "chunked") {
          check(sizes.get(entry.blob) === entry.size, "file content missing or length mismatch");
          continue;
        }
        const children = parseJSON(await this.get("blob", entry.blob));
        check(Array.isArray(children) && children.length >= 2 && children.length <= 1024, "invalid chunk manifest");
        const files = children.map(file);
        let total = 0;
        for (const child of files) {
          check(!child.kind?.startsWith("symlink") && !child.executable && child.size > 0 && child.size < entry.size, "invalid chunk child");
          total += child.size;
          check(Number.isSafeInteger(total), "chunk length overflow");
        }
        check(total === entry.size, "chunk manifest length mismatch");
        entries.push(...files);
      }
    }
  }

  async publish(value: unknown): Promise<string> {
    const tx = transaction(value), bytes = transactionBytes(tx);
    check(bytes.length <= METADATA_LIMIT, "transaction exceeds 2 MiB");
    const id = objectID("transaction", bytes);
    const journal = this.journal(shard(this.project, "transaction", id));
    if ((await journal.info([id]))[0] !== null) return id;
    const parents = new Map<string, Transaction>();
    const dependencies = [...new Set(Object.values(tx.edits).flatMap(edit => edit.parents))];
    await map(dependencies, async parent => {
      parents.set(parent, transaction(parseJSON(await this.get("transaction", parent))));
    });
    for (const [path, edit] of Object.entries(tx.edits)) {
      for (const parent of edit.parents) check(Object.hasOwn(parents.get(parent)!.edits, path), "dependency did not write this path");
    }
    await this.verifyFiles([...Object.values(tx.edits).flatMap(edit => edit.value ? [edit.value] : []), ...(tx.provenance ? [tx.provenance] : [])]);
    await this.env.OBJECTS.put(this.key("transaction", id), bytes);
    await journal.append(id, bytes.length, Object.keys(tx.edits));
    return id;
  }

  async sync(cursors: number[], paths: string[]) {
    const pages = await map(cursors.map((cursor, index) => ({ cursor, index })), ({ cursor, index }) => this.journal(index).page(cursor, paths));
    return {
      transactions: [...new Set(pages.flatMap(page => page.transactions))].sort(),
      cursors: pages.map(page => page.cursor), more: pages.some(page => page.more),
    };
  }

  async publishPin(value: unknown): Promise<void> {
    const release = pin(value), bytes = pinBytes(release);
    check(bytes.length <= METADATA_LIMIT, "release pin exceeds 2 MiB");
    const graph = new Map<string, Transaction>();
    const todo = [...release.roots];
    while (todo.length) {
      const id = todo.pop()!;
      if (graph.has(id)) continue;
      const tx = transaction(parseJSON(await this.get("transaction", id)));
      graph.set(id, tx);
      todo.push(...Object.values(tx.edits).flatMap(edit => edit.parents));
    }
    const heads = new Map<string, Map<string, FileEntry | null>>();
    for (const [id, tx] of graph) {
      for (const [path, edit] of Object.entries(tx.edits)) {
        if (!heads.has(path)) heads.set(path, new Map());
        heads.get(path)!.set(id, edit.value);
      }
    }
    for (const tx of graph.values()) {
      for (const [path, edit] of Object.entries(tx.edits)) {
        for (const parent of edit.parents) heads.get(path)!.delete(parent);
      }
    }
    const files: [string, FileEntry][] = [];
    for (const [path, values] of heads) {
      const alternatives = [...values.values()];
      check(alternatives.every(value => JSON.stringify(value) === JSON.stringify(alternatives[0])), "release view contains conflicts");
      if (alternatives[0]) files.push([path, alternatives[0]]);
    }
    check(snapshotJSON(Object.fromEntries(files)) === snapshotJSON(release.snapshot.files), "release view does not match committed roots");
    const id = objectID("pin", bytes);
    await this.env.OBJECTS.put(this.key("pin", id), bytes);
    await this.journal(shard(this.project, "pin", id)).pin(id, bytes.length);
  }

  async pins(after: string): Promise<Pin[]> {
    const pages = await map(Array.from({ length: SHARDS }, (_, i) => i), i => this.journal(i).pins(after));
    const ids = [...new Set(pages.flat())].sort().slice(0, BATCH_LIMIT);
    return map(ids, async id => pin(parseJSON(await this.get("pin", id))));
  }
}
