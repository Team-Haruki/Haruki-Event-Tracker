# WebSocket vs HTTP for web leaderboards

Status: option **B** and the server-side quick wins are implemented (see [Implemented](#implemented) and [Rollout](#rollout)). The frontend switch is still to do.
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

## Implemented

Server side of **B**, the overview split into cacheable parts, and the WS proxy quick win. Everything is additive: the current Toolbox frontend (WS request frames with `_t`) keeps working byte for byte.

### `updated` carries `version`

```json
{"type":"updated","server":"cn","eventId":180,"timestamp":1760000000,"version":4242}
```

- `version` is the event's API-cache epoch **after** the bump that caused the push:
  - reader (`cluster/subscriber.rs::invalidate`): the `INCR` result of `finish_event_update`, which now returns it;
  - standalone writer (`tracker/daemon.rs`): `CacheInvalidation::finish` returns the bumped epoch, the tracker base keeps the last one (`EventTrackerBase::cache_version`) and the daemon's push carries it.
- Omitted when the process has no API cache (or the bump failed). A cluster writer has no WS clients, so it never sends one.
- **A reader announces only versions whose data it holds.** The epoch is bumped on every update, but `version` is pushed only if the writer sent its WAL position and the reader's database has replayed it. With `replica_wait_ms > 0` the reader waits up to that long. With `0` it probes once. A primary, or a non-Postgres engine, counts as replayed.
  - If replay misses the deadline, or there is no position (non-Postgres writer, LSN read failure, gap resync), the push carries no `version`.
  - Otherwise, a fetch right after the push could read the replica's old rows, the cache would accept them under the new epoch, and they would be served `immutable` for that `v`. Worst case, that is an event's final ranking pinned wrong for a day.
  - Clients treat a missing `version` as "refresh unversioned".
- `realtime.push_min_interval_secs` coalescing keeps the latest `(timestamp, version)` pair for the trailing push.

### HTTP caching on `/api/v2/web/...` (`src/api/http_cache.rs`)

One middleware (`web_cache_headers`) on the public router, outside `CompressionLayer` and inside the access log (so a `304` is logged as `304`):

| Case | `Cache-Control` | `ETag` |
| --- | --- | --- |
| `200`, request has `v=<version>` and the body is the cache entry for exactly that version | `public, max-age=86400, immutable` | yes |
| any other `200` (no `v`, stale/future `v`, endpoint that doesn't report a version) | `public, max-age=1, stale-while-revalidate=5` | yes |
| `/private/` routes, raw-UID lookups (`check-room`, `details/user/{uid}` resolved as a game UID) | `private, no-store` | no |
| non-`200` | `no-store` | no |

- **ETag:** strong, `"<first 128 bits of SHA-256 of the bytes sent>"`. It is computed after compression, so identity, gzip and br each get their own tag, and `Vary: accept-encoding` is always set. `Content-Length` is set on the buffered body.
- **`If-None-Match`:** weak comparison (`W/` prefix and `*` accepted, lists allowed). A hit answers `304` with `ETag`, `Cache-Control` and `Vary`, and no body.
- **Which version was served:** the API cache now tags `CachedJson` with the epoch its bytes belong to. The tag is set only when the bytes came from the epoch-keyed cache (L1/L2 hit) or were accepted into it (the write script checks the epoch is unchanged and not dirty), and it is cleared for dirty bypasses, rejected writes, Redis errors and oversize values. `EncodedJson` carries it as a `ServedEpoch` response extension. So "immutable" means "the bytes are the ones stored for epoch `v`", not just "`v` equals the current epoch" — a stale L1 control can't label older bytes with a newer `v`, and a `v` that differs from what was served always falls back to the short lifetime.
- Only the live overviews and overview parts (`top100`, `borders`, `growth` without `at`; not the wall-clock `overview`) report a version today, and only on the precompressed path, which is what browsers get (`Accept-Encoding` includes gzip). Everything else gets the short lifetime even with `v`.
- `v` is ignored by the handlers, so it never reaches the server-side cache key. The key already contains the epoch.
- WS request frames go through `web_v2_routes` without this layer and are unchanged.

### Overview split into parts

The ~128 KB overview is also served as three resources, so a view fetches only what it shows, and each part has its own `ETag` and version caching:

| Endpoint (under `/api/v2/web/events/{server}/{eventId}/leaderboards/`) | Body |
| --- | --- |
| `total/top100`, `world-bloom/{characterId}/top100` | `{meta, topRankings}` |
| `total/borders`, `world-bloom/{characterId}/borders` | `{meta, borderLines}` |
| `total/growth`, `world-bloom/{characterId}/growth` | `{meta, topPlayerGrowths, topRankGrowths, borderGrowths, intervalSeconds, windowStart, windowEnd}` |
| `total/status`, `world-bloom/{characterId}/status` | `{meta, status?}`, computed per request and never versioned |

- **Query params** (all parts): `interval` (seconds, default 3600, clamped to 1–86400), `at` (unix seconds, replay; no `at` = live), `v` (the `version` from `updated`). `v` only affects `Cache-Control`.
- **Field shapes** (camelCase, same as in the overview):
  - `meta`: `{server, eventId, scope, characterId?, fetchedAt}`.
  - `topRankings[]`: `{rankData, userData?}`, the same items as the overview.
  - `status` (status endpoint only): `{timestamp, status, statusDesc, timeAgo}`; left out when there is no heartbeat.
  - `borderLines[]`: `{rank, score, timestamp}`.
  - `topPlayerGrowths[]`: `{rank, userId, scoreLatest, timestampLatest, scoreEarlier, timestampEarlier, timeDiff, growth, characterId?}`.
  - `topRankGrowths[]` and `borderGrowths[]`: `{rank, timestampLatest, scoreLatest, timestampEarlier?, scoreEarlier?, timeDiff?, growth?}`.
  - The lists are always present (`[]` when empty).
- **Epoch-pure bodies:** a versioned body may depend only on the data behind its epoch, never on the clock. Otherwise the same `v` recomputes to different bytes after the 1 s TTL while the first copy stays cached for a day.
  - The parts are cut (`to_object_iter` + raw splice) from an "as-of" overview cached per version under `web:v2:asof:…`. It is built by the same builders (`build_overview_until` / `build_world_bloom_overview_until`), reading at #80's per-epoch commit cut (`resolve_rank_cut`: the newest stored `time_id`, pinned per cache epoch and part of the cache key), with the window end and `meta.fetchedAt` set to **that cut's sample timestamp**. Ranking rows only land together with an epoch bump; heartbeat rows don't count.
  - `status` is left out of the parts because idle and error heartbeats change it without a bump, and `timeAgo` is wall-clock. It has its own per-request `status` endpoint.
  - Tests assert that two fetches of one `v` more than a TTL apart are byte-identical, and that all parts of one `v` share `meta.fetchedAt` (= `growth.windowEnd`).
  - Clients should pass the same `interval` to every part so `top100` / `borders` share the as-of overview that `growth` computes.
- **The old `overview` stays wall-clock:** its window ends at `now` and its `status` carries `timeAgo`, so it is never marked immutable, whatever `v` says. Its body is unchanged.
- **Why parts are cut from one overview and not the other way round:** it keeps the old `overview` byte-identical, keeps one query plan per version, and leaves the overview builders (and #80's `rank_snapshot_rows` cut + per-user dedupe they call) as the only source of the data.
- **Old `overview`:** unchanged and still served (HTTP and WS request frames). It is deprecated for new clients. Remove it only after the Toolbox has switched and the access log shows no more `.../overview` hits without `at`.
- **Caveats:**
  - Identity responses (clients without gzip) are never immutable: only the precompressed path carries the epoch.
  - `?at=` responses get the short lifetime. They are historical, but an `at` ahead of the last flushed sample can still change, so no long max-age is given.
  - Epochs live in each reader's API-cache Redis. Readers behind one hostname must either share that Redis or be sticky per client. Otherwise a `version` from reader A names a different epoch on reader B, and B simply answers with the short lifetime (safe, just uncached).

### WS proxy splices the body

`handle_proxy_request` no longer parses the body into a `sonic_rs::Value` and serializes it again. It validates the body (`LazyValue`, a skip-scan) and writes `{"id":…,"ok":true,"data":<body>,"status":…}` around the bytes. A test runs every public web endpoint through both paths and compares the text byte for byte, with awkward ids and a sonic-encoded payload full of escapes, floats and unicode. The one divergence: the old parse turned `-0.0` into `0.0`. The splice keeps the handler's `-0.0`, as the HTTP body always did.

### `push_min_interval_secs`

The default stays `0`. The recommendation is **2–5 for CN** on the process that serves the Toolbox sockets (the CN reader). Pushes are coalesced with a trailing update that carries the latest timestamp and version, so the last change is never lost. The overview request rate, and so the traffic, drops by the same factor. The example config says so.

## How Toolbox requests reach the tracker

Recorded from `Team-Haruki/Haruki-Toolbox-Backend` (`external/oathkeeper/*.yml`, `docker-compose.yml`) and the frontend `.env`. The live edge was not probed.

1. The frontend uses `VITE_HARUKI_EVENT_TRACKER_URL=https://toolbox-api-direct.haruki.seiunx.com/event-tracker`. That is a different origin from the Toolbox site, so every call is a CORS request.
2. A TLS edge for `toolbox-api-direct` (not in the repo; presumably on the Toolbox host CN02) forwards to Oathkeeper's proxy (`:4455`).
3. Oathkeeper matches `/event-tracker/...` and proxies to `http://haruki-toolbox-event-tracker:8777` with `strip_path: /event-tracker`:
   - `ws-ticket`, `ws`, `api/v2/web/events/.../private/...`: `cookie_session` authenticator plus the `header` mutator (subject headers).
   - Public web rule (`noop`): `total|world-bloom/{c}` × `overview`, `replay/overview`, `details/rank/*`, `details/user/*`, `users/search`. **`check-room` has no public rule**, so it is reachable only through WS request frames. **The new `top100|borders|growth|status` endpoints have no rule yet**: until the regex gains them, Oathkeeper answers `404 Requested url does not match any rules` (WS request frames still reach them).
   - Rules match the path, so `?v=` and `_t` pass through.
4. **CORS** is Oathkeeper's (`serve.proxy.cors`, origin from `FRONTEND_PUBLIC_URL`, credentials allowed). `exposed_headers` is only `Content-Type`, and `allowed_headers` doesn't include `If-None-Match`. That is fine as long as the frontend lets the browser HTTP cache do the revalidation: the browser adds `If-None-Match` itself, turns a `304` into a `200` from cache, and JS never needs to read `ETag`. Only if the frontend sets `If-None-Match` by hand (which forces a preflight) or reads `ETag` must those be added to `allowed_headers` / `exposed_headers`.
5. **304 / ETag through the proxies:** Oathkeeper is a Go `httputil.ReverseProxy` and passes `ETag`, `Cache-Control` and `304` through unchanged. If the TLS edge is Caddy, `encode` skips responses that already carry `Content-Encoding`. The tracker compresses anything over 1 KiB for gzip/br clients, so the edge shouldn't re-encode (and so shouldn't rewrite the `ETag`). Verify once on the live edge with `curl -sI -H 'Accept-Encoding: gzip' …overview` and then again with `If-None-Match`.
6. There is no shared cache on this path (the `-cdn` EdgeOne host isn't used for the tracker). `public` lets one be added later, and the browser cache benefits immediately.

## Rollout

1. **Tracker:** merge #77 and then this change, and deploy the CN reader behind the Toolbox (`haruki-toolbox-event-tracker`) and the other trackers. Nothing else has to change at the same time: old clients ignore `version`, WS request frames are unchanged, and REST calls only gain headers.
2. **Config (optional, anytime):** `realtime.push_min_interval_secs: 2`–`5` on the CN reader.
3. **Oathkeeper (Toolbox-Backend):** add `top100|borders|growth|status` to both alternatives of the `haruki-public-tracker-web-v2-prefixed` rule, e.g. `total/(overview|replay/overview|top100|borders|growth|status|details/rank/[^/]+|details/user/[^/]+|users/search)` and the same for `world-bloom/[^/]+/(…)`, then deploy the rules (see the ops runbook: back up, replace, restart Oathkeeper, and probe).
4. **Verify the edge:** `ETag` present, `If-None-Match` → `304`, `Content-Encoding: gzip` on `top100`, and `immutable` only with the current `v` (take it from a live `updated` push).
5. **Toolbox frontend:**
   - On `updated` with a `version`, fetch the parts the view shows, `GET …/{top100|borders|growth}?interval=…&v=<version>`, over HTTP with the browser's default cache mode. Use the same `interval` for all of them. Drop `_t` and `cache: "no-store"`.
   - Without a `version`, fetch the same URLs without `v` (short-lived).
   - Tracker health: `GET …/status` (short-lived; `status.timestamp`, `timeAgo`). The parts no longer carry `status`, and their `meta.fetchedAt` is the data's as-of time, not the request time.
   - If `version` is missing (an older tracker), keep the WS request frame.
   - Pause refreshes while `document.hidden` and refresh once on `visibilitychange`.
   - Don't set `If-None-Match` or read `ETag` by hand; otherwise extend the Oathkeeper CORS headers first.
6. **Later:** remove the live `overview` once nothing requests it. Once no client uses WS request frames for public data, restrict WS request frames to what needs the socket's subject (the private endpoints and `check-room`).

