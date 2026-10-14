// A Worker shaped like cairn's sync: one state object, and a history it
// lists and prunes. Only for testing Henry's scope.js.
export const VERSION = 3;
let n = 0;

export default {
  key(url) { return url.searchParams.get("key"); },
  async fetch(req, env) {
    const url = new URL(req.url);
    if (url.pathname === "/state" && req.method === "PUT") {
      const old = await env.BUCKET.get("state.json");
      if (old) await env.BUCKET.put(`history/${++n}`, await old.text());
      await env.BUCKET.put("state.json", await req.text());
      return new Response("ok");
    }
    if (url.pathname === "/state") return new Response(await (await env.BUCKET.get("state.json")).text());
    if (url.pathname === "/history" && req.method === "DELETE") {
      const listed = await env.BUCKET.list({ prefix: "history/" });
      await env.BUCKET.delete(listed.objects.map((o) => o.key));
      return new Response("ok");
    }
    if (url.pathname === "/history") {
      const listed = await env.BUCKET.list({ prefix: "history/" });
      return new Response(JSON.stringify(listed.objects.map((o) => o.key)));
    }
    if (url.pathname === "/dirs") {
      const listed = await env.BUCKET.list({ delimiter: "/" });
      return new Response(JSON.stringify(listed.delimitedPrefixes));
    }
    if (url.pathname === "/raw") {
      const o = await env.BUCKET.get(this.key(url));
      return new Response(o ? await o.text() : "none");
    }
    if (url.pathname === "/token") return new Response(env.SYNC_TOKEN);
    return new Response("?", { status: 404 });
  },
};
