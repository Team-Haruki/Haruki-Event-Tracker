# Web API Capabilities

This document tracks the web-facing API surface on top of the existing Bot-compatible event API.

## Current Web Capabilities

The web API is mounted under:

```text
GET /event/{server}/{event_id}/web/...
```

It is designed for public website usage. It requires `privacy.uid_anonymization.enabled = true`; public responses and lookups use `unique_id` as `userId` and never expose raw upstream UID.

### Ranking Search

```text
GET /event/{server}/{event_id}/web/rankings
GET /event/{server}/{event_id}/web/world-bloom-rankings/character/{character_id}
```

Supported filters:

- `rankMin`, `rankMax`
- `scoreMin`, `scoreMax`
- `startTime`, `endTime`
- `before`, `after`
- `timestamp`
- `limit`
- `cursor`

Responses use:

```json
{
  "items": [],
  "nextCursor": "timestamp:rank:user_id_key"
}
```

Normal ranking rows return `timestamp`, `userId`, `score`, and `rank`. World Bloom rows also include `characterId`.

### User Trace Search

```text
GET /event/{server}/{event_id}/web/trace-ranking/user/{unique_id}
GET /event/{server}/{event_id}/web/trace-world-bloom-ranking/character/{character_id}/user/{unique_id}
```

Supported filters:

- `startTime`
- `endTime`
- `cursor`
- `limit`

These endpoints are intended for charting a single public user over a bounded time window.

### User Profile Search

```text
GET /event/{server}/{event_id}/web/users
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
- Nearest snapshot query:
  - fetch the latest ranking snapshot at or before a requested timestamp.
  - better for timeline scrubbing than strict `timestamp`.
- Rank-range leaderboard pages:
  - stable browsing for ranges such as T1-T100, T1000-T5000.
  - consider cursor plus jump-to-rank support.
- Trace downsampling:
  - support chart-friendly sampling windows such as 5m, 15m, 1h.
  - cap maximum returned points.
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
  - query-hash cache for flexible web filters.
  - stricter limits for fuzzy profile search.
- Public API v2 documentation:
  - document Bot-compatible legacy endpoints separately from web endpoints.
  - make privacy behavior explicit.

## Privacy Defaults

- Public website APIs should only accept and return `unique_id`.
- Raw UID remains internal database data for deduplication and maintenance.
- `twitterId` should stay out of persistence and API responses unless a separate privacy review approves it.
- Logs and cache keys for web endpoints should use public IDs and query filters only.
