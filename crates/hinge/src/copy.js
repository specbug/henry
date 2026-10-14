// Written by Henry, and removed again within minutes: copies every object
// in FROM into TO under PREFIX, one batch per request, never overwriting an
// object already there. Answers only to Henry's one-time TOKEN.
const BATCH = 50;

export default {
  async fetch(req, env) {
    if (req.method !== "POST" || req.headers.get("Authorization") !== `Bearer ${env.TOKEN}`) {
      return new Response("no", { status: 401 });
    }
    const { cursor } = await req.json();
    const listed = await env.FROM.list({ limit: BATCH, ...(cursor ? { cursor } : {}) });
    let copied = 0;
    let kept = 0;
    for (const o of listed.objects) {
      const to = env.PREFIX + o.key;
      if (await env.TO.head(to)) {
        kept++;
        continue;
      }
      const body = await env.FROM.get(o.key);
      if (!body) continue;
      await env.TO.put(to, await body.arrayBuffer(), {
        httpMetadata: body.httpMetadata,
        customMetadata: body.customMetadata,
      });
      copied++;
    }
    return Response.json({ copied, kept, cursor: listed.truncated ? listed.cursor : null });
  },
};
