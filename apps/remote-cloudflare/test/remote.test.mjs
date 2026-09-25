import assert from "node:assert/strict";
import { before, after, test } from "node:test";
import { createHash, randomBytes } from "node:crypto";
import { zstdCompressSync } from "node:zlib";
import { mkdtemp, mkdir, readFile, writeFile, rm, symlink } from "node:fs/promises";
import { tmpdir } from "node:os";
import { resolve, join } from "node:path";
import { execFile } from "node:child_process";
import { promisify } from "node:util";
import { unstable_dev } from "wrangler";

const exec = promisify(execFile);
const binary = process.env.KELP_BIN ?? resolve("../../target/debug", process.platform === "win32" ? "kelp.exe" : "kelp");
const token = "cloudflare-test-token";
let root, worker;
const options = () => ({
  config: resolve("wrangler.jsonc"), port: 0, inspectorPort: 0, local: true,
  persistTo: join(root, "storage"), vars: { KELP_TOKEN: token }, logLevel: "error",
  experimental: { disableExperimentalWarning: true, disableDevRegistry: true, watch: false },
});

before(async () => {
  root = await mkdtemp(join(tmpdir(), "kelp-cloudflare-"));
  worker = await unstable_dev("src/index.ts", options());
}, { timeout: 60_000 });
after(async () => {
  await worker?.stop();
  if (root) await rm(root, { recursive: true, force: true });
});

async function request(path, method = "GET", body) {
  return worker.fetch(path, { method, headers: { authorization: `Bearer ${token}` }, body });
}
async function json(path, method, data) {
  const response = await request(path, method, data === undefined ? undefined : JSON.stringify(data));
  assert.ok(response.ok, `${method} ${path}: ${response.status} ${await response.clone().text()}`);
  return response.status === 204 ? null : response.json();
}
async function kelp(directory, ...args) {
  const { stdout } = await exec(binary, ["-C", directory, "--json", ...args], {
    env: { ...process.env, KELP_TOKEN: token }, timeout: 90_000, maxBuffer: 8 * 1024 * 1024,
  });
  return JSON.parse(stdout);
}
const hash = (kind, bytes) => createHash("sha256").update(`kelp/0\0${kind}\0${bytes.length}\0`).update(bytes).digest("hex");
const tx = (edits, nonce = "test") => ({ format: 1, nonce, message: "Atomic change", edits });
const id = value => hash("transaction", Buffer.from(JSON.stringify(value)));
const endpoint = project => `/v1/projects/${project}`;
const url = project => `http://${worker.address}:${worker.port}/${project}`;

test("authentication, immutable objects, atomic publication and scoped journals", async () => {
  assert.equal((await worker.fetch("/healthz")).status, 200);
  assert.equal((await worker.fetch(endpoint("demo"), { method: "PUT" })).status, 401);
  assert.equal((await request(endpoint("demo"))).status, 404);
  const project = await json(endpoint("demo"), "PUT");
  assert.equal(project.protocol, "kelp/1");
  assert.equal(project.storage_nodes, 16);
  const bytes = Buffer.from("file bytes"), blob = hash("blob", bytes);
  const entry = { blob, size: bytes.length, executable: false };
  const transaction = tx({ "api/a": { parents: [], value: entry }, "lib/b": { parents: [], value: entry } });
  const base = endpoint("demo");
  assert.equal((await request(`${base}/transactions`, "POST", JSON.stringify(transaction))).status, 400);
  assert.deepEqual((await json(`${base}/sync`, "POST", { cursors: [] })).transactions, []);
  assert.equal((await request(`${base}/objects/blob/${blob}`, "PUT", "wrong bytes")).status, 400);
  assert.equal((await request(`${base}/objects/blob/${blob}`, "PUT", bytes)).status, 204);
  assert.equal((await request(`${base}/objects/blob/${blob}`, "HEAD")).headers.get("content-length"), String(bytes.length));
  for (let i = 0; i < 2; i++) assert.deepEqual(await json(`${base}/transactions`, "POST", transaction), { transaction: id(transaction) });
  const synced = await json(`${base}/sync`, "POST", { cursors: [], paths: ["api"] });
  assert.deepEqual(synced.transactions, [id(transaction)]);
  assert.deepEqual((await json(`${base}/sync`, "POST", { cursors: synced.cursors })).transactions, []);
  assert.deepEqual((await json(`${base}/sync`, "POST", { cursors: [], paths: ["api-old"] })).transactions, []);
  const invalid = tx({ "other": { parents: [id(transaction)], value: null } }, "bad-parent");
  assert.equal((await request(`${base}/transactions`, "POST", JSON.stringify(invalid))).status, 400);
  assert.equal((await request(`${base}/objects/transaction/${id(invalid)}`)).status, 404);
  await json(endpoint("separate"), "PUT");
  assert.equal((await request(`${endpoint("separate")}/objects/blob/${blob}`)).status, 404);
  const continuation = tx({ "api/a": { parents: [id(transaction)], value: null } }, "next");
  assert.equal((await request(`${endpoint("separate")}/transactions`, "POST", JSON.stringify(continuation))).status, 404);
  await json(`${base}/transactions`, "POST", continuation);
  const inventedPin = { name: "bad", snapshot: { files: {} }, roots: [id(transaction)] };
  assert.equal((await request(`${base}/pins`, "POST", JSON.stringify(inventedPin))).status, 400);
  assert.deepEqual(await json(`${base}/pins`, "GET"), []);
});

test("malformed and oversized batches cannot publish or bypass hash validation", async () => {
  const base = endpoint("invalid");
  await json(base, "PUT");
  assert.equal((await request(`${base}/objects/upload`, "POST", "not zstd")).status, 400);
  const bomb = zstdCompressSync(Buffer.alloc(32 * 1024 * 1024 + 1));
  assert.equal((await request(`${base}/objects/upload`, "POST", bomb)).status, 400);
  const invalidKind = Buffer.alloc(73);
  invalidKind.write("KLP1");
  invalidKind[4] = 1;
  invalidKind.write("0".repeat(64), 5);
  assert.equal((await request(`${base}/objects/upload`, "POST", zstdCompressSync(invalidKind))).status, 400);
  assert.equal((await request(`${base}/sync`, "POST", JSON.stringify({ cursors: [0] }))).status, 400);
  const duplicate = { kind: "blob", id: "0".repeat(64) };
  assert.equal((await request(`${base}/objects/info`, "POST", JSON.stringify({ objects: [duplicate, duplicate] }))).status, 400);
  const manifest = Buffer.from(JSON.stringify([
    { blob: "1".repeat(64), size: 2, executable: false },
    { blob: "2".repeat(64), size: 2, executable: false },
  ]));
  const manifestID = hash("blob", manifest);
  assert.equal((await request(`${base}/objects/blob/${manifestID}`, "PUT", manifest)).status, 204);
  const incomplete = { ...tx({ file: { parents: [], value: { blob: manifestID, size: 4, executable: false, kind: "chunked" } } }), format: 2 };
  assert.equal((await request(`${base}/transactions`, "POST", JSON.stringify(incomplete))).status, 400);
  assert.deepEqual((await json(`${base}/sync`, "POST", { cursors: [] })).transactions, []);
});

test("real CLI: chunked files, sparse sync, conflicts, pins, and durable restart", { timeout: 180_000 }, async () => {
  const remote = url("integration");
  await kelp(root, "init", "alice", "--project", "integration", "--remote", remote);
  const alice = join(root, "alice");
  await mkdir(join(alice, "api"));
  await mkdir(join(alice, "assets"));
  // Numeric paths and Unicode key order catch canonical JSON/hash drift.
  for (const path of ["10", "2", "__proto__", "api/\uE000", "api/\u{10000}", "api/file"]) {
    await writeFile(join(alice, path), `contents for ${path}`);
  }
  if (process.platform !== "win32") await symlink("../2", join(alice, "api/link"));
  if (process.platform === "linux") await writeFile(Buffer.concat([Buffer.from(join(alice, "api/raw-")), Buffer.from([255])]), "raw filename");
  const large = randomBytes(21 * 1024 * 1024);
  await writeFile(join(alice, "assets/large"), large);
  for (let i = 0; i < 140; i++) await writeFile(join(alice, "api", `${i}.txt`), `small file ${i}`);
  const initial = await kelp(alice, "commit", "-m", "Atomic cross-directory contents");
  await kelp(alice, "tag", "v1.0");
  await kelp(alice, "push");
  await writeFile(join(alice, "outside"), "independent history");
  const outside = await kelp(alice, "commit", "-m", "Unrelated file");
  await kelp(alice, "push");
  await kelp(root, "clone", remote, "partial", "--paths", "api");
  const partial = join(root, "partial");
  await assert.rejects(readFile(join(partial, "assets/large")), { code: "ENOENT" });
  assert.ok(!(await kelp(partial, "log", "--commits")).some(entry => entry.commit === outside.transaction));
  await writeFile(join(partial, "api/file"), "partial edit");
  await kelp(partial, "commit", "-m", "Selected edit");
  await kelp(partial, "push");
  await kelp(alice, "pull");
  assert.equal(await readFile(join(alice, "api/file"), "utf8"), "partial edit");
  await kelp(partial, "pull", "--paths", ".");
  assert.deepEqual(await readFile(join(partial, "assets/large")), large);
  assert.equal((await kelp(partial, "tag"))[0].view, initial.view);
  await writeFile(join(alice, "api/file"), "alice conflict");
  await writeFile(join(partial, "api/file"), "partial conflict");
  await kelp(alice, "commit", "-m", "Alice alternative");
  await kelp(partial, "commit", "-m", "Partial alternative");
  await Promise.all([kelp(alice, "push"), kelp(partial, "push")]);
  await assert.rejects(kelp(partial, "pull"), /conflict/);
  await kelp(partial, "pull", "--keep-local");
  await kelp(partial, "commit", "-m", "Resolve both parents");
  await kelp(partial, "push");
  const all = await json(`${endpoint("integration")}/sync`, "POST", { cursors: [] });
  assert.ok(all.cursors.filter(cursor => cursor > 0).length > 1, "one project should span journal shards");
  await worker.stop();
  worker = await unstable_dev("src/index.ts", options());
  await kelp(root, "clone", url("integration"), "restarted");
  const restarted = join(root, "restarted");
  assert.equal(await readFile(join(restarted, "api/file"), "utf8"), "partial conflict");
  await kelp(restarted, "restore", "v1.0");
  assert.equal(await readFile(join(restarted, "api/file"), "utf8"), "contents for api/file");
  assert.deepEqual(await readFile(join(restarted, "assets/large")), large);
  await kelp(restarted, "commit", "-m", "Restore release");
  await kelp(restarted, "push");
});
