const SEGMENT_CACHE_TTL = 600; // 10 minutes, redis keeps a 5 min cache in case of cloudflare error

export default {
  async fetch(request, env, ctx): Promise<Response> {
    const url = new URL(request.url);

    if (!url.pathname.startsWith("/api/v1/proxy")) {
      return fetch(request);
    }

    const schema = url.searchParams.get("schema");

    if (schema !== "sports") {
      return fetch(request);
    }

    const exp = url.searchParams.get("exp");
    if (exp) {
      const expiry = parseInt(exp, 10);
      if (!isNaN(expiry) && expiry < Math.floor(Date.now() / 1000)) {
        return fetch(request);
      }
    }

    const encodedUrl = url.searchParams.get("url");
    if (!encodedUrl) {
      return fetch(request);
    }

    const cacheKeyUrl = new URL(request.url);
    cacheKeyUrl.search = "";
    cacheKeyUrl.searchParams.set("url", encodedUrl);
    cacheKeyUrl.searchParams.set("schema", "sports");
    const cacheKey = new Request(cacheKeyUrl.toString(), {
      method: "GET",
      headers: request.headers,
    });

    // why does my lsp say caches.default does not have this field, yes it does fuck you
    const cache = caches.default;
    const cachedResponse = await cache.match(cacheKey);
    if (cachedResponse) {
      return cachedResponse;
    }

    const originHeaders = new Headers(request.headers);
    originHeaders.delete("Accept-Encoding");

    const originRequest = new Request(request.url, {
      method: request.method,
      headers: originHeaders,
    });

    const originResponse = await fetch(originRequest);

    if (!originResponse.ok) {
      return originResponse;
    }

    const contentType = originResponse.headers.get("Content-Type") || "";
    const isSegment =
      contentType.includes("video/mp2t") || contentType.includes("video/mp4");

    if (!isSegment) {
      return originResponse;
    }

    const responseToCache = new Response(originResponse.body, originResponse);
    responseToCache.headers.set(
      "Cache-Control",
      `public, max-age=${SEGMENT_CACHE_TTL}`,
    );

    ctx.waitUntil(cache.put(cacheKey, responseToCache.clone()));

    return responseToCache;
  },
} satisfies ExportedHandler;
