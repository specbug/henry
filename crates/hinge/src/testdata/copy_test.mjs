// Runs Henry's copy.js against two in-memory R2 buckets. `node copy_test.mjs`
// in a directory holding copy.js; exits non-zero on a failure.
import assert from "node:assert/strict";

class Bucket {
  objects = new Map();
  async head(k) { return this.objects.has(k) ? { key: k } : null; }
  async get(k) {
    const v = this.objects.get(k);
    return v === undefined ? null : { key: k, httpMetadata: { contentType: "application/json" }, customMetadata: {}, arrayBuffer: async () => new TextEncoder().encode(v).buffer };
  }
  async put(k, v) { this.objects.set(k, typeof v === "string" ? v : new TextDecoder().decode(v)); }
  async list({ limit = 1000, cursor } = {}) {
    const keys = [...this.objects.keys()].sort();
    const start = cursor ? Number(cursor) : 0;
    const page = keys.slice(start, start + limit);
    const truncated = start + limit < keys.length;
    return { objects: page.map((key) => ({ key })), truncated, cursor: truncated ? String(start + limit) : undefined };
  }
}

const from = new Bucket();
for (let i = 0; i < 60; i++) from.objects.set(`history/state-${String(i).padStart(3, "0")}.json`, `h${i}`);
from.objects.set("state.json", "old");
const to = new Bucket();
to.objects.set("apps/cairn/sync/BUCKET/state.json", "written on Henry");
to.objects.set("henry/restic/config", "backups");
const env = { FROM: from, TO: to, PREFIX: "apps/cairn/sync/BUCKET/", TOKEN: "t" };
const { default: copier } = await import("./copy.js");
const post = async (body, token = "t") => {
  const res = await copier.fetch(new Request("https://c.example/", { method: "POST", headers: { Authorization: `Bearer ${token}` }, body: JSON.stringify(body) }), env);
  return [res.status, res.status === 200 ? await res.json() : null];
};

assert.equal((await post({}, "wrong"))[0], 401);
let cursor = null, copied = 0, kept = 0;
do {
  const [status, r] = await post({ cursor });
  assert.equal(status, 200);
  copied += r.copied; kept += r.kept; cursor = r.cursor;
} while (cursor);
assert.equal(copied, 60);
assert.equal(kept, 1, "what the app wrote on Henry is kept");
assert.equal(to.objects.get("apps/cairn/sync/BUCKET/state.json"), "written on Henry");
assert.equal(to.objects.get("apps/cairn/sync/BUCKET/history/state-059.json"), "h59");
assert.equal(to.objects.get("henry/restic/config"), "backups");
assert.equal(from.objects.size, 61, "the source is only read");
console.log("copy ok");
