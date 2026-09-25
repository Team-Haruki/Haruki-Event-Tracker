# Web API Capabilities

This document tracks the web-facing API surface on top of the existing Bot-compatible (cloud) event API.

## Current Web Capabilities

The web API is mounted under:

```text
GET /api/v2/web/events/{server}/{event_id}/leaderboards/...
```

It is designed for public website usage. It requires `privacy.uid_anonymization.enabled = true`; public responses and lookups use `unique_id` as `userId` and never expose raw upstream UID.

The v1-era standalone search endpoints (`/event/.../web/rankings`, `/web/trace-ranking/...`) are no longer mounted; their query capabilities (cursor-paginated ranking search and user/rank traces, implemented in `db::query::web`) are served through the leaderboard overview/detail endpoints below.

### Leaderboard Overview And Replay

```text
GET .../leaderboards/total/overview
GET .../leaderboards/total/replay/overview
GET .../leaderboards/world-bloom/{character_id}/overview
GET .../leaderboards/world-bloom/{character_id}/replay/overview
```

Query params: `interval` (trace sampling window in seconds, default 3600, clamped to 1–86400) and `at` (unix timestamp for timeline scrubbing / replay playback). Overview responses are served from the two-tier API cache, optionally as precompressed gzip. `v=<version>` (the `version` of the last realtime `updated` push) is accepted and ignored by the query itself; it only affects `Cache-Control` (below).

### Overview parts

```text
GET .../leaderboards/total/top100
GET .../leaderboards/total/borders
GET .../leaderboards/total/growth
GET .../leaderboards/world-bloom/{character_id}/top100
GET .../leaderboards/world-bloom/{character_id}/borders
GET .../leaderboards/world-bloom/{character_id}/growth
GET .../leaderboards/total/status
GET .../leaderboards/world-bloom/{character_id}/status
```

Parts of the overview as separate resources, each with its own `ETag` and version caching. The query params are the overview's (`interval`, `at`, `v`).

- `top100`: `{meta, topRankings: WebRankingItem[]}`
- `borders`: `{meta, borderLines: {rank, score, timestamp}[]}`
- `growth`: `{meta, topPlayerGrowths[], topRankGrowths[], borderGrowths[], intervalSeconds, windowStart, windowEnd}`
- `status`: `{meta, status?: {timestamp, status, statusDesc, timeAgo}}`. It is computed per request and never versioned.

The lists are always present (`[]` when empty).

The three data parts are a pure function of the data behind a cache version:
- They are cut from one cached "as-of" overview per version.
- Live, their `meta.fetchedAt` and growth window end at the **newest ranking sample**, not the wall clock. With `at`, they end at `at`.
- They carry no `status`, whose heartbeat moves without an epoch bump.

So every fetch of one `v` is byte-identical, and all parts of one `v` share `meta.fetchedAt`. Pass the same `interval` to every part: `top100` and `borders` don't depend on it, but sharing it lets them reuse the as-of overview that `growth` needs. Staleness ("updated N s ago") comes from `status.timestamp` (or `timeAgo`) on the `status` endpoint.

The full `overview` stays available with an unchanged body: wall-clock window, `status` with `timeAgo`. It is deprecated for new clients, never marked immutable, and removal will be announced once the Toolbox no longer uses it.

### HTTP caching (all `/api/v2/web/...` GETs)

- `200` responses carry a strong `ETag` (digest of the bytes actually sent, so gzip / br / identity each have their own) and `Vary: accept-encoding`; `If-None-Match` with a matching tag (weak comparison, `*` allowed) answers `304` with the same `ETag` / `Cache-Control` / `Vary`.
- `Cache-Control: public, max-age=86400, immutable` when the request has `v=<version>` and the body is the API cache's entry for exactly that version. Only the live `top100` / `borders` / `growth` parts fetched with gzip accepted qualify; the precompressed variant carries its epoch. Identity (non-precompressed) responses, `at` requests, the overview and `status` never do.
- `Cache-Control: public, max-age=1, stale-while-revalidate=5` otherwise — no `v`, a `v` that is not the served version, or an endpoint that doesn't report one.
- `Cache-Control: private, no-store` and no `ETag` for the `/private/` routes and raw-UID lookups (`check-room`, `details/user/{uid}` resolved as a game UID); `no-store` for non-`200` answers.

Top-100 rows (and every rank snapshot: rank details' `current`/`previous`/`next`, the cloud `sk` endpoints) are read at one cut — the newest committed ranking row, pinned per API-cache epoch so separate requests answered in the same epoch agree. A rank whose latest row names a player who has a newer row at another rank is stale (the tracker stores only changed ranks) and is omitted rather than listing that player twice; clients render it as unknown until the tracker rewrites it. A user detail (or cloud `sk` query by `userId`) for a player who has dropped out of the tracked ranks is a 404 rather than the player now holding their last rank.

### Rank / User Details

```text
GET .../leaderboards/total/details/rank/{rank}
GET .../leaderboards/total/details/user/{user_id}
GET .../leaderboards/world-bloom/{character_id}/details/rank/{rank}
GET .../leaderboards/world-bloom/{character_id}/details/user/{user_id}
```

`{user_id}` accepts either the public `unique_id` or a positive numeric game UID (a bare numeric id is always treated as a game UID; `idType=unique` / `idType=uid` force the interpretation). A game UID is mapped to the event-specific anonymous ID before querying, so detail cache keys still use anonymous IDs, but the response then reveals that one player's raw UID (see the next section). This lookup does not require a Toolbox binding. Query params: `interval`, `at`, `includeTrace`, `includePlayerTrace`, `includeProfile`, `cursor`, `limit` (trace pages are cursor-paginated).

A trace with no rows in the requested window — typically a `cursor` poll with nothing newer — is an empty `rankTrace` / `playerTrace`, not a 404; the detail 404s only when the rank has never been held or the player was never tracked in the event.

User details carry `ranked`. A player tracked in the event who no longer holds a tracked rank gets `"ranked": false` with `current`, `previous` and `next` omitted (their last row's rank belongs to someone else now, who is never shown in their place), while `playerTrace` and `profile` still describe them. The private user details follow the same rule. The cloud `sk/query?userId=` keeps answering 404 for such a player.

### Exact UID Lookup (check-room)

```text
GET .../leaderboards/total/check-room?userId={raw_uid}
GET .../leaderboards/world-bloom/{character_id}/check-room?userId={raw_uid}
GET .../leaderboards/total/details/user/{raw_uid}?idType=uid
GET .../leaderboards/world-bloom/{character_id}/details/user/{raw_uid}?idType=uid
```

The one deliberate exception to "web never accepts a raw UID": the caller types an exact upstream UID and gets that player's current rank with neighbours, using the same query params as the user detail. The response is the user detail shape plus a `subject` block (`{"userId": "<raw>", "uniqueId": "<unique_id>"}`); the subject's own `userId` fields (current row, trace rows, profile) are the raw UID, while `previous` / `next` and every other player stay `unique_id`. The raw UID is validated as a ≤30-digit number and mapped to `unique_id` by the anonymizer (no lookup), so it never enters cache keys, tracing fields, or the access log (the `check-room` query string is not logged). 404 when the UID is not tracked in that event.

### Private Details (raw UID)

```text
GET .../leaderboards/total/private/details/user/{user_id}
GET .../leaderboards/world-bloom/{character_id}/private/details/user/{user_id}
```

Guarded by `private::require_subject`: the subject comes from the WebSocket proxy extension or trusted-proxy (Oathkeeper) headers, and ownership of `(server, user_id)` is verified against the Toolbox backend (`toolbox` config). 401 without a subject.

### Realtime (WebSocket)

```text
GET /ws-ticket
GET /ws?ticket=...
```

`/ws-ticket` issues a single-use 45-second ticket to subjects resolved from trusted-proxy headers. The socket accepts `subscribe` / `unsubscribe` / `ping` frames plus proxied requests for any `/api/v2/web/...` path, and pushes `ready` / `updated` / `online` events for subscribed `(server, event_id)` topics. Tracker writes trigger the `updated` broadcasts: `{"type":"updated","server":"cn","eventId":180,"timestamp":1760000000,"version":4242}`. `version` is the event's API-cache epoch after the write. It is omitted when the process has no API cache, and on a cluster reader until its database has confirmably replayed the write (WAL position reached): an unconfirmed update still refreshes caches, but announces no version; fetch `...?v=<version>` over HTTP to get a cacheable response for exactly that data. Proxied request frames keep their `{"id","ok","status","data"}` reply unchanged.

### User Profile Search

```text
GET .../leaderboards/total/users/search
GET .../leaderboards/world-bloom/{character_id}/users/search
```

Supported filters:

- `uniqueId`
- `name`
- `profileWord`
- `cardId`
- `cardLevel`
- `cardMasterRank`
- `cardSpecialTrainingStatus`
- `cardDefaultImage`
- `cheerfulTeamId`
- `cursor`
- `limit`

At least one search filter is required. `name` and `profileWord` require at least two characters.

Returned user data currently includes:

- `userId` (`unique_id`)
- `name`
- `cheerfulTeamId`
- card fields: `cardId`, `cardLevel`, `cardMasterRank`, `cardSpecialTrainingStatus`, `cardDefaultImage`
- `profileWord`
- `profileHonors`
- `userPlayerFrames`

`twitterId` is intentionally not stored or exposed.

### Storage And Indexes

New event tables include indexes for common web reads:

- normal ranking: `(rank, time_id)`, `(user_id_key, time_id)`, `(time_id, rank)`, `(time_id, score)`
- World Bloom ranking: `(character_id, rank, time_id)`, `(character_id, user_id_key, time_id)`, `(character_id, time_id, rank)`
- users: `unique_id`, `name`, `card_id`, `cheerful_team_id`

Existing historical tables receive user/profile column lazy migration through the API path, but large ranking-table index backfills should be handled as an explicit operational migration.

## Planned Web Capabilities

### High Priority

- Event list and event detail APIs:
  - filter by server, event id, event status, event type, unit, time range, World Bloom chapter, and character.
  - persist historical event metadata instead of relying only on current tracker state.
- ~~Nearest snapshot query~~ — shipped: overview/replay-overview `at` param resolves the latest snapshot at or before the requested timestamp.
- Rank-range leaderboard pages:
  - stable browsing for ranges such as T1-T100, T1000-T5000.
  - consider cursor plus jump-to-rank support.
- ~~Trace downsampling~~ — shipped: `interval` sampling param (1s–24h) on overview and detail endpoints.
- User/rank comparison:
  - compare multiple `unique_id` values or rank lines over the same time window.

### Medium Priority

- Honor and player-frame filtering:
  - current data is stored as JSON for display.
  - high-performance filtering should use normalized index tables.
- Better name search:
  - prefix search, case normalization, kana/width normalization where useful.
  - stable sorting by recent appearance or best match.
- Custom score growth analytics:
  - user growth over arbitrary windows.
  - rank bucket growth.
  - final-rush interval stats.
- World Bloom aggregation:
  - unified normal/chapter response shapes.
  - per-character chapter summary and comparison.
- User profile change history:
  - preserve name/card/profile changes over time, especially post-event refresh changes.

### Long Term

- Event archive search across events:
  - search a public user across historical events.
  - expose per-event best rank/score summary.
- Precomputed analytics:
  - popular ranking lines, growth windows, score distributions, and final results.
  - reduce online query load for website dashboards.
- Cache and rate-limit policy:
  - query-hash cache shipped (`api/cache.rs`, epoch-versioned L1 + Redis L2); trace-query concurrency limiting shipped (`api/limiter.rs`).
  - still open: per-IP rate limiting and stricter limits for fuzzy profile search.
- Public API v2 documentation:
  - document Bot-compatible legacy endpoints separately from web endpoints.
  - make privacy behavior explicit.

## Privacy Defaults

- Public website APIs should only accept and return `unique_id`; the exact-UID check-room above is the sole opt-in exception and reveals only the queried player.
- `privacy.uid_anonymization.enabled` is mandatory on cluster writers so every user gets a `unique_id` at write time; the cloud group (`/api/v2/cloud/*`, bearer-token gated via `cloud_api.tokens`) keeps speaking raw UIDs regardless of that switch.
- Raw UID remains internal database data for deduplication and maintenance.
- `twitterId` should stay out of persistence and API responses unless a separate privacy review approves it.
- Logs and cache keys for web endpoints should use public IDs and query filters only.
