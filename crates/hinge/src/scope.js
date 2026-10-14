// Written by Henry. This Worker's R2 bindings are Henry's bucket, and the
// Worker sees only its own corner of it: every key it reads, writes, lists
// or deletes is under a prefix, and keys come back without it. The bucket
// itself never reaches the Worker's code, so it cannot touch Henry's backups
// or another app's data.
import worker from "__MAIN__";
export * from "__MAIN__";

const PREFIXES = __PREFIXES__;
const HANDLERS = ["fetch", "scheduled", "queue", "email", "tail", "trace"];

function scope(bucket, prefix) {
  const key = (k) => prefix + k;
  const strip = (k) => (typeof k === "string" && k.startsWith(prefix) ? k.slice(prefix.length) : k);
  // R2's objects answer for their key; theirs says where it really is.
  const object = (o) =>
    o &&
    new Proxy(o, {
      get(target, prop) {
        if (prop === "key") return strip(target.key);
        const value = Reflect.get(target, prop, target);
        return typeof value === "function" ? value.bind(target) : value;
      },
    });
  return {
    head: async (k) => object(await bucket.head(key(k))),
    get: async (k, options) => object(await bucket.get(key(k), options)),
    put: async (k, value, options) => object(await bucket.put(key(k), value, options)),
    delete: (k) => bucket.delete(Array.isArray(k) ? k.map(key) : key(k)),
    list: async (options = {}) => {
      const scoped = { ...options, prefix: prefix + (options.prefix ?? "") };
      if (options.startAfter !== undefined) scoped.startAfter = key(options.startAfter);
      const listed = await bucket.list(scoped);
      return {
        ...listed,
        objects: listed.objects.map(object),
        delimitedPrefixes: (listed.delimitedPrefixes ?? []).map(strip),
      };
    },
    createMultipartUpload: (k, options) => bucket.createMultipartUpload(key(k), options),
    resumeMultipartUpload: (k, id) => bucket.resumeMultipartUpload(key(k), id),
  };
}

const envs = new WeakMap();
function envFor(env) {
  let out = envs.get(env);
  if (!out) {
    out = { ...env };
    for (const [name, prefix] of Object.entries(PREFIXES)) out[name] = scope(env[name], prefix);
    envs.set(env, out);
  }
  return out;
}

if (typeof worker !== "object" || worker === null) {
  throw new Error("Henry scopes R2 only for a Worker whose default export is an object of handlers");
}
const scoped = Object.create(worker);
for (const h of HANDLERS) {
  if (typeof worker[h] === "function") {
    scoped[h] = (first, env, ...rest) => worker[h].call(worker, first, envFor(env), ...rest);
  }
}
export default scoped;
