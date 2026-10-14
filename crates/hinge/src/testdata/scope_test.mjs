// Runs Henry's scope.js in front of a Worker, against an in-memory R2 bucket
// shaped like Cloudflare's. `node scope_test.mjs` in a directory holding the
// generated henry-entry.js and src/worker.js; exits non-zero on a failure.
import assert from "node:assert/strict";

class R2Object {
  #key; #body;
  constructor(key, body) { this.#key = key; this.#body = body; }
  get key() { return this.#key; }
  get size() { return this.#body.length; }
  get httpEtag() { return `"${this.#body.length}"`; }
  async text() { return this.#body; }
}

class Bucket {
  objects = new Map();
  async head(k) { return this.objects.has(k) ? new R2Object(k, this.objects.get(k)) : null; }
  async get(k) { return this.head(k); }
  async put(k, v) { this.objects.set(k, String(v)); return new R2Object(k, String(v)); }
  async delete(k) { for (const key of [k].flat()) this.objects.delete(key); }
  async list({ prefix = "", startAfter, delimiter } = {}) {
    const keys = [...this.objects.keys()].filter((k) => k.startsWith(prefix) && (!startAfter || k > startAfter)).sort();
    if (!delimiter) return { objects: keys.map((k) => new R2Object(k, this.objects.get(k))), truncated: false, delimitedPrefixes: [] };
    const prefixes = new Set();
    const objects = [];
    for (const k of keys) {
      const rest = k.slice(prefix.length);
      const i = rest.indexOf(delimiter);
      if (i >= 0) prefixes.add(prefix + rest.slice(0, i + 1));
      else objects.push(new R2Object(k, this.objects.get(k)));
    }
    return { objects, truncated: false, delimitedPrefixes: [...prefixes] };
  }
}

const bucket = new Bucket();
bucket.objects.set("henry/restic/config", "Henry's backups");
bucket.objects.set("apps/other/sync/BUCKET/state.json", "another app's data");
const env = { BUCKET: bucket, SYNC_TOKEN: "t" };
const { default: worker } = await import("./henry-entry.js");
const call = async (method, path, body) =>
  (await worker.fetch(new Request(`https://w.example${path}`, { method, body }), env, {})).text();

// What the Worker writes lands under its prefix, and it reads it back.
assert.equal(await call("PUT", "/state", "v1"), "ok");
assert.equal(await call("PUT", "/state", "v2"), "ok");
assert.equal(bucket.objects.get("apps/cairn/sync/BUCKET/state.json"), "v2");
assert.equal(await call("GET", "/state"), "v2");

// Its listing sees its own keys, without the prefix; pruning deletes only them.
assert.deepEqual(JSON.parse(await call("GET", "/history")), ["history/1"]);
assert.deepEqual(JSON.parse(await call("GET", "/dirs")), ["history/"]);
await call("DELETE", "/history");
assert.deepEqual(JSON.parse(await call("GET", "/history")), []);

// Nothing outside its corner is reachable, however it asks.
assert.equal(await call("GET", "/raw?key=../../other/sync/BUCKET/state.json"), "none");
assert.equal(await call("GET", "/raw?key=henry/restic/config"), "none");
assert.equal(bucket.objects.get("henry/restic/config"), "Henry's backups");
assert.equal(bucket.objects.get("apps/other/sync/BUCKET/state.json"), "another app's data");

// Its other exports and helpers still work, and other bindings pass through.
const mod = await import("./henry-entry.js");
assert.equal(mod.VERSION, 3);
assert.equal(await call("GET", "/token"), "t");
console.log("scope ok");
