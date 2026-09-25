import { timingSafeEqual } from "node:crypto";
import { Buffer } from "node:buffer";
import { Journal } from "./journal";
import { Store } from "./store";
import {
  BLOB_LIMIT, HttpError, METADATA_LIMIT, PACK_LIMIT, body, check, decodePack, hash, keys, name, parseJSON, syncRequest,
} from "./protocol";

export { Journal };
export interface Env {
  OBJECTS: R2Bucket;
  JOURNALS: DurableObjectNamespace<Journal>;
  KELP_TOKEN: string;
}

export default {
  async fetch(request: Request, env: Env): Promise<Response> {
    try {
      const url = new URL(request.url), method = request.method;
      if (url.pathname === "/healthz" && method === "GET") return new Response("ok\n");
      if (!env.KELP_TOKEN?.trim()) throw new HttpError(503, "configure the KELP_TOKEN Worker secret");
      const supplied = Buffer.from(request.headers.get("authorization") ?? "");
      const expected = Buffer.from(`Bearer ${env.KELP_TOKEN}`);
      if (supplied.length !== expected.length || !timingSafeEqual(supplied, expected)) throw new HttpError(401, "a valid bearer token is required");
      const parts = url.pathname.split("/");
      if (parts[1] !== "v1" || parts[2] !== "projects" || !parts[3]) throw new HttpError(404, "route not found");
      const store = new Store(env, name(parts[3]));
      const route = parts.slice(4).join("/");
      if (!route && ["GET", "PUT"].includes(method)) return Response.json(await store.projectInfo(method === "PUT"));
      await store.projectInfo(false);
      if (route === "sync" && method === "POST") {
        const { cursors, paths } = syncRequest(parseJSON(await body(request, METADATA_LIMIT)));
        return Response.json(await store.sync(cursors, paths));
      }
      if (route === "transactions" && method === "POST") {
        return Response.json({ transaction: await store.publish(parseJSON(await body(request, METADATA_LIMIT))) });
      }
      if (route === "pins") {
        if (method === "GET") {
          const after = url.searchParams.get("after") ?? "";
          if (after) hash(after);
          return Response.json(await store.pins(after));
        }
        if (method === "POST") {
          await store.publishPin(parseJSON(await body(request, METADATA_LIMIT)));
          return new Response(null, { status: 204 });
        }
      }
      if (route === "objects/upload" && method === "POST") {
        await store.uploadPack(decodePack(await body(request, PACK_LIMIT + 1024 * 1024)));
        return new Response(null, { status: 204 });
      }
      if (["objects/info", "objects/download"].includes(route) && method === "POST") {
        const requested = keys(parseJSON(await body(request, METADATA_LIMIT)));
        if (route === "objects/info") return Response.json(await store.info(requested));
        return new Response(await store.download(requested), { headers: { "content-type": "application/x-kelp-pack" } });
      }
      if (parts[4] === "objects" && parts.length === 7) {
        const kind = parts[5], id = hash(parts[6]);
        check(kind === "blob" || kind === "transaction", "invalid object kind");
        if (method === "PUT") {
          check(kind === "blob", "transactions must use the publication endpoint");
          await store.upload(id, await body(request, BLOB_LIMIT));
          return new Response(null, { status: 204 });
        }
        if (method === "HEAD") {
          const info = (await store.info([{ kind, id }]))[0];
          if (info.size === null) throw new HttpError(404, "object is missing");
          return new Response(null, { headers: { "content-length": String(info.size) } });
        }
        if (method === "GET") return new Response(await store.get(kind, id), { headers: { "content-type": "application/octet-stream" } });
      }
      throw new HttpError(404, "route not found");
    } catch (error) {
      const status = error instanceof HttpError ? error.status : 503;
      if (!(error instanceof HttpError)) console.error(error);
      const code = status === 401 ? "UNAUTHORIZED" : status === 404 ? "NOT_FOUND" : status >= 500 ? "UNAVAILABLE" : "INVALID";
      return Response.json({ code, message: error instanceof HttpError ? error.message : "remote storage is unavailable; retry the request" }, { status });
    }
  },
} satisfies ExportedHandler<Env>;
