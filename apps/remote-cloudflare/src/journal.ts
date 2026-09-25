import { DurableObject } from "cloudflare:workers";
import type { Env } from "./index";

// Each project has independent hash-partitioned journals. R2 payloads are
// written first; this atomic receipt is the publication point readers observe.
export class Journal extends DurableObject<Env> {
  constructor(ctx: DurableObjectState, env: Env) {
    super(ctx, env);
    ctx.storage.sql.exec(`
      CREATE TABLE IF NOT EXISTS transactions (
        sequence INTEGER PRIMARY KEY AUTOINCREMENT, id TEXT NOT NULL UNIQUE, size INTEGER NOT NULL
      );
      CREATE TABLE IF NOT EXISTS paths (path TEXT NOT NULL, id TEXT NOT NULL, PRIMARY KEY(path,id));
      CREATE INDEX IF NOT EXISTS paths_id ON paths(id);
      CREATE TABLE IF NOT EXISTS pins (id TEXT PRIMARY KEY, size INTEGER NOT NULL);
    `);
  }

  info(ids: string[]): (number | null)[] {
    return ids.map(id => this.ctx.storage.sql.exec<{ size: number }>(
      "SELECT size FROM transactions WHERE id=?", id,
    ).toArray()[0]?.size ?? null);
  }

  async append(id: string, size: number, paths: string[]): Promise<void> {
    this.ctx.storage.transactionSync(() => {
      if (this.info([id])[0] !== null) return;
      this.ctx.storage.sql.exec("INSERT INTO transactions(id,size) VALUES(?,?)", id, size);
      for (const path of paths) this.ctx.storage.sql.exec("INSERT INTO paths(path,id) VALUES(?,?)", path, id);
    });
    await this.ctx.storage.sync();
  }

  page(after: number, paths: string[]): { transactions: string[]; cursor: number; more: boolean } {
    const rows = this.ctx.storage.sql.exec<{ sequence: number; id: string }>(
      `SELECT sequence,id FROM transactions t WHERE sequence>? AND
       (?='[]' OR EXISTS(SELECT 1 FROM paths p, json_each(?) s WHERE p.id=t.id AND
         (p.path=s.value OR (p.path>=s.value||'/' AND p.path<s.value||'0')
          OR (s.value>=p.path||'/' AND s.value<p.path||'0'))))
       ORDER BY sequence LIMIT 257`, after, JSON.stringify(paths), JSON.stringify(paths),
    ).toArray();
    const more = rows.length > 256;
    if (more) rows.pop();
    return { transactions: rows.map(row => row.id), cursor: rows.at(-1)?.sequence ?? after, more };
  }

  async pin(id: string, size: number): Promise<void> {
    this.ctx.storage.sql.exec("INSERT OR IGNORE INTO pins(id,size) VALUES(?,?)", id, size);
    await this.ctx.storage.sync();
  }

  pins(after: string): string[] {
    return this.ctx.storage.sql.exec<{ id: string }>("SELECT id FROM pins WHERE id>? ORDER BY id LIMIT 128", after)
      .toArray().map(row => row.id);
  }
}
