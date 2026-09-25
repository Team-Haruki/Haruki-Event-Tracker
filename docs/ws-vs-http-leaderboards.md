# WebSocket vs HTTP for web leaderboards

Status: investigation and design. No protocol change yet.
Trigger: the CN event 180 Toolbox reader logs about 4.7 `.../total/overview` requests per second (2801 per 10 min). The browser shows no HTTP `GET` for them, and the data arrives over an uncompressed WebSocket.

## Findings

### What the socket does today

- **Connecting:**
  1. The client calls `GET /ws-ticket` (Oathkeeper subject headers, single-use 45 s ticket).
  2. It then opens `GET /ws?ticket=...` (`src/api/router.rs`).
  3. The server sends `{"type":"ready","subject":…,"online":{"total":N,"topic":0}}`.
- **Client frames** (`src/api/ws.rs`, `WsRequest`), all shaped `{id, type?, path?, server?, eventId?}`:
  - `{"id","type":"subscribe","server":"cn","eventId":180}`, plus `unsubscribe` and `ping` in the same shape.
  - Any frame without a `type` is a request: `{"id","path":"/api/v2/web/..."}`.
- **Server pushes** (`WsEvent`) are small notifications of a few dozen bytes:
  - `{"type":"updated","server","eventId","timestamp"}`
  - `{"type":"online","server","eventId","online":{…}}`
  - No leaderboard data and no diffs are pushed.
- **What triggers `updated`:**
  - On the writer, every tracker flush that wrote rows (`tracker/daemon.rs`).
  - On a reader, every cluster update after the API-cache epoch bump (`cluster/subscriber.rs::invalidate`: `finish_event_update`, then `notify_update`).
  - `realtime.push_min_interval_secs` can coalesce pushes per topic, but it defaults to `0`. CN writes top-100 rows every second, so each subscriber gets about one `updated` per second.
- **Request frames are virtual HTTP requests** (`handle_proxy_request`):
  1. The path becomes a synthetic `GET` with no headers, only a `PrivateSubject` extension.
  2. The request is run through `web_v2_routes` wrapped in `access_log::log`, using `tower::ServiceExt::oneshot`.
  3. The response body is read into memory (up to 8 MiB) and parsed into a `sonic_rs::Value`.
  4. The value is serialized again inside `{"id","ok":true,"status":200,"data":…}` and sent as a text frame.
  - All of this is done separately for every request on every socket. Nothing is shared between subscribers.
- **Toolbox frontend** (`Team-Haruki/Haruki-Toolbox`, `src/modules/rank-border/`):
  - `TrackerWsClient` keeps one socket per endpoint and subscribes to `(server, eventId)`.
  - On every `updated` event, `useRankBorderLive` calls `refreshData(true)`. That is a single-flight overview request sent as a WS request frame, with a `_t=<ms>` cache-buster appended.
  - The socket is only used for logged-in users (`hasActiveSession`). Logged-out visitors make one REST call per page load or interval change.
  - There is no `document.hidden` check, so background tabs keep refreshing every second.
- **Toolbox backend:** it does not relay anything. Oathkeeper proxies `/event-tracker/{ws,ws-ticket,api/v2/web/...}` straight to the tracker.

### Why the access log shows "HTTP" overview requests

The WS request frames go through the same `access_log::log` middleware as real requests. A virtual request has no `ConnectInfo` and no proxy headers, so the client-IP column is `-`:

- `-` in the client-IP column means the request came over the socket.
- A real address means it came over REST.

The logged path also carries the frontend's `_t` cache-buster. The server's cache key does not include it.

In practice, the 4.7 req/s are roughly 5 logged-in pages, each asking for the full overview after every push.

### Compression and caching

- **permessage-deflate is not available.**
  - axum 0.8 uses `tokio-tungstenite`/`tungstenite` 0.29. That version has no deflate extension.
  - axum's upgrade response never sends `Sec-WebSocket-Extensions`, so the extension is never negotiated.
  - The direct `tokio-tungstenite = "0.30"` dependency is only used by the cluster client and tests.
  - Neither Oathkeeper nor Caddy adds compression to WebSocket frames.
- **HTTP responses are compressed.**
  - The public router has `CompressionLayer` (gzip and br, quality 4, responses over 1 KiB).
  - The live overview is additionally served from a pre-gzipped cache variant (`cached_overview_bytes` → `EncodedJson::gzip`).
  - WS virtual requests carry no `Accept-Encoding`, so they always get the identity bytes.
- **No conditional caching.** No endpoint sets `ETag`, `Last-Modified` or `Cache-Control`, and none handles `If-None-Match` or returns `304`. The frontend's REST path also uses `cache: "no-store"`.
- **Server-side cache is already per write version.** API cache values are keyed by the per-event epoch (`haruki:tracker:<server>:<event>:api_cache:v<epoch>:<suffix>`), which every flush increments. So the overview is already cached until the next flush, or for its 1 s TTL, whichever comes first.
  - A longer TTL would not help CN. The epoch changes every second anyway, and the live overview's window (`now - interval .. now`) moves with the clock.
  - The miss rate (about 260/min) is one computation per epoch per distinct suffix: interval, total vs. World Bloom chapter, and replay.

### Payload size

Measured on a synthetic CN overview: 100 `topRankings` with profile honors and frames, about 100 `topPlayerGrowths`, 100 `topRankGrowths`, and 19 border lines and 19 border growths.

| Encoding | Bytes per overview |
| --- | --- |
| identity (what the WS sends today) | ~128 KB (`topRankings` alone is ~86 KB) |
| gzip -4 (HTTP `CompressionLayer`) | ~22 KB |
| gzip -6 (≈ permessage-deflate, if it existed) | ~20 KB |
| br -4 (HTTP `CompressionLayer`) | ~16 KB |

Real data repeats more (names, honors), so it should compress somewhat better than this synthetic payload.

At the observed 4.7 overview requests/s:

| | Throughput | Per minute | Per hour |
| --- | --- | --- | --- |
| Today (identity) | ~600 KB/s (~4.8 Mbit/s) | ~36 MB | ~2.2 GB |
| Same traffic compressed | ~75–105 KB/s | ~5–6 MB | — |

Both grow linearly with the number of logged-in pages. The tracker also parses and re-serializes each 128 KB body per request, about 600 KB/s of JSON work today. Deflate across messages would not help much: the payload is larger than the 32 KB deflate window.

## Options

### A — permessage-deflate on the existing protocol

Negotiate `permessage-deflate` on `/ws` so each frame is compressed transparently.

- **Pros**
  - No client code change: browsers negotiate and inflate automatically.
  - The message schema stays the same.
  - Cuts per-push bytes about 6x.
- **Cons**
  - Not possible with the current stack. axum's `ws` (tungstenite 0.29) has no deflate support, so this means swapping the WS implementation for a deflate-capable crate or adding a proxy that implements the extension.
  - Compression runs per socket and per message, so CPU grows with subscribers. Nothing is shared.
  - The shape stays "one full overview per subscriber per second" and nothing can be cached by HTTP.
  - Context takeover buys little at 128 KB per message.
- **Compatibility:** transparent where negotiated. Clients or proxies that don't offer the extension keep getting identity frames.
- **Alternative inside A:** app-level compression (binary frames holding gzip or br bytes, inflated with `DecompressionStream`). This works with the current stack but is a protocol change, and the client has to opt in.

### B — notifications on WS, data over cacheable HTTP (recommended)

This is the operator's original model. The socket carries only change notifications, and clients fetch data with ordinary `GET`s.

- **Server changes**
  - `updated` carries the cache version: `{"type":"updated","server","eventId","timestamp","version":<epoch>}`. On readers the epoch is already bumped before the push (`finish_event_update` can return the `INCR` result). On the writer the push happens after the invalidation's `finish_event_update`.
  - Overview (and other web `GET`) responses get:
    - `ETag: "<epoch>-<hash(suffix)>"`, or a hash of the cached bytes.
    - `Cache-Control: public, max-age=1, stale-while-revalidate=5` on unversioned URLs.
    - `Cache-Control: public, max-age=86400, immutable` when the request carries `v=<epoch>`, since the response for a version never changes.
    - A `304` for a matching `If-None-Match`.
  - The body keeps coming from the pre-compressed cache: gzip today, br optional. The server already stores one compressed copy per epoch.
- **Client changes**
  - On `updated`, `GET .../overview?interval=3600&v=<version>` with the browser's normal `Accept-Encoding`.
  - Drop the `_t` cache-buster and `cache: "no-store"`.
  - Skip refreshes while `document.hidden`, and do one refresh on `visibilitychange`.
- **Pros**
  - Responses shrink about 6–8x (br/gzip).
  - Identical requests for the same version are served from the cache and, with versioned URLs, can also be served by Caddy, a CDN or the browser cache, never reaching the tracker.
  - `304`s make retries and duplicate tabs almost free.
  - Access logs show real client IPs again.
  - The WS stays tiny and cheap to fan out.
  - Reuses the existing epoch cache and precompression. No new dependencies.
- **Cons**
  - One extra round trip between notification and data (tens of ms).
  - More HTTP requests through Oathkeeper. The overview is public, so its rule can skip the session check.
  - The frontend has to change before any bandwidth is saved.
  - The live overview window is computed at first request for a version. This is fine: a version lasts about 1 s on CN.
- **Compatibility and migration**
  1. Server: add `version` to `updated` (an additive field; old clients ignore it), and add ETag, `Cache-Control` and `304` handling to web `GET`s. Also accept `v`, but leave it out of the server cache key: the key already contains the epoch.
  2. Toolbox: switch the refresh to HTTP with `v`. Keep WS request frames as a fallback for one release.
  3. Optional: once no client uses them, restrict WS request frames to endpoints that truly need the socket, such as private endpoints resolved through the socket's subject.

### C — hybrid: server-pushed, encode-once overview frames

Clients subscribe with the view they render, for example `{"type":"subscribe","server","eventId","views":["total/overview?interval=3600"]}`. On each version the server builds that overview once, serializes it once, and sends the same frame to every subscriber of that view. Optionally the frame is a binary gzip or br body taken straight from the cache.

- **Pros**
  - Lowest latency: no request round trip.
  - Serialization happens once per `(view, version)` instead of per subscriber.
  - With binary compressed frames, bytes match B without any HTTP.
- **Cons**
  - The biggest protocol change: new subscribe shape and frame type, plus a client decoder.
  - The server tracks per-view subscriptions.
  - Pushes still go to hidden tabs unless the client unsubscribes.
  - HTTP caches and CDNs can't help.
  - Per-view state has to be managed on reconnect.
- **Compatibility:** new message types behind a capability flag in `subscribe`. Old clients keep the request-frame flow.

### Quick wins that need no protocol change

These are independent of the choice above:

- Set `realtime.push_min_interval_secs` to 2–5 for CN. This is config only. The pushes are coalesced with a trailing update, so the last change is never lost. It divides the overview request rate, and so the traffic, by the same factor.
- In `handle_proxy_request`, splice the response bytes into the reply frame instead of parsing into a `sonic_rs::Value` and re-serializing: write `{"id":…,"ok":true,"status":…,"data":` + body + `}`. The wire format stays identical, and a 128 KB parse and re-encode per request goes away.
- In the frontend, pause auto-refresh on hidden tabs.

## Recommendation

1. Apply the quick wins now: `push_min_interval_secs` for CN, the raw-bytes splice in the WS proxy, and the hidden-tab pause.
2. Adopt **B** as the target design. It matches the intended model, reuses the per-epoch precompressed cache, gets br/gzip and `304`s for free, and opens the way to Caddy or CDN caching through versioned URLs. The only protocol addition is one optional field.
3. Keep **C** in reserve in case the extra round trip ever matters. Don't pursue **A**: the current WebSocket stack doesn't support it, and it would still ship a full overview per subscriber per second.
