# `vacuum-finished-events` — freezing finished events by hand

Operator subcommand that runs `VACUUM (FREEZE, ANALYZE)` over the tables of
events that are over, **one event at a time**. It is never scheduled by the
daemon; the only way it runs is an operator invoking it.

## Why

Event tables are append-only while an event runs and untouched once it is
closed. Autovacuum therefore never visits them for dead tuples, only for
anti-wraparound freezing once `age(relfrozenxid)` crosses
`autovacuum_freeze_max_age` (200 M transactions by default). The tables
imported in one batch on 2026-09-15 all carry xids from the same window, so
they would cross that threshold together: one burst of freeze WAL across
hundreds of tables at whatever time of day the counter happens to get there,
with the replica lagging behind it. Freezing the finished events now, off-peak
and sequentially, spreads that cost out and leaves each relation with a fresh
`relfrozenxid`, so autovacuum has nothing left to do for them.

## What counts as finished

For each event whose `event_<id>_time_id` table exists in the region's
database, the subcommand needs **both**:

1. The master-data entry (`events.json` under the region's
   `master_data_dir`, the same file the tracker reads) has `closedAt` in the
   past. After `closedAt` the tracker never selects the event again
   (`get_current_event_status` requires `now < closedAt`), so no writer
   touches its tables. Events that have a time table but no master entry are
   skipped and counted in the summary line.
2. The time table's newest `timestamp` is older than `--min-idle-days`
   (default 3). This guards against a stale `events.json` and against any
   late writer, whatever master data says. `closedAt` itself must also be
   older than that.

With `--idle-only` the master data is not read and rule 2 alone decides. Use
it when the region's master data is unreachable from the machine you run on;
raise `--min-idle-days` to a week or more in that mode.

Tables are vacuumed in the order `event_<id>`, `wl_<id>` (World Bloom only),
`event_<id>_users`, `event_<id>_time_id`, whichever of them exist.

## Usage

```text
haruki-event-tracker vacuum-finished-events [--region <jp|en|tw|kr|cn>] [--event <id>]
    [--dry-run] [--max-events <n>] [--min-idle-days <days>] [--pause-secs <secs>]
    [--min-xid-age <n>] [--idle-only] [--config <uri>]
```

| Flag | Default | Meaning |
|---|---|---|
| `--region` | every enabled region | One region only. Regions run one after another, sorted by name. |
| `--event <id>` | all | Restrict to one event (it still has to qualify as finished). |
| `--dry-run` | off | List the selected events and their tables with `age(relfrozenxid)` and size; run nothing. |
| `--max-events <n>` | unlimited | Stop after this many events, across regions. |
| `--min-idle-days <d>` | 3 | Idle guard on the time table. |
| `--pause-secs <s>` | 5 | Sleep between tables and between events. `0` disables. |
| `--min-xid-age <n>` | 0 | Skip tables whose `age(relfrozenxid)` is already below this, so a re-run only touches what still needs freezing. |
| `--idle-only` | off | Ignore master data, select by idleness alone. |
| `--config <uri>` | `HARUKI_CONFIG_URI` / `haruki-tracker-configs.yaml` | Same config the daemon uses; only `servers.<region>.gorm_config` and `master_data_dir` are read. |

The DSN must point at the **primary**: `VACUUM` is refused on a standby and
the subcommand exits with `database is in recovery` if pointed at one. The
replica receives the freeze through WAL. On a region whose dialect is not
PostgreSQL the subcommand prints `PostgreSQL only ... nothing to do` and
exits 0.

Exit codes: 0 done (or nothing to do), 1 a region failed (config, connection,
master data, standby, SQL error), 2 bad arguments. The output is plain text on
stdout; nothing is written to the daemon's log files.

## Running it in production

1. Pick a low-traffic window. Every table produces freeze WAL roughly
   proportional to its size, and `VACUUM` takes a `SHARE UPDATE EXCLUSIVE`
   lock (reads and the writer's inserts are not blocked; DDL and other
   vacuums on the same table are).
2. Start with `--dry-run` and check the list. The `xid_age` column tells you
   how far each table is from the freeze threshold; the sizes tell you how
   much WAL to expect.
3. Run for real with a bounded batch, for example
   `--max-events 5 --pause-secs 10`, and watch the replica while it runs:
   `/readyz` on the reader reports `replicationLagSecs`, or on the standby
   `SELECT now() - pg_last_xact_replay_timestamp()`. If lag climbs past what
   the reader's `replica_wait_ms` tolerates, stop (Ctrl-C between tables is
   safe; a vacuum interrupted mid-table simply leaves that table for the
   next run) and use a longer pause or a smaller batch.
4. Repeat until the dry run lists nothing with a meaningful `xid_age`. A
   later re-run with `--min-xid-age 1000000` (or similar) skips everything
   already frozen and only picks up events that have finished since.

Do not put this in cron or a systemd timer. The point of the subcommand is
that a human chooses the moment and watches the replica.

## Verifying

```sql
SELECT relname, age(relfrozenxid), last_vacuum, last_analyze
FROM pg_class c JOIN pg_stat_user_tables s USING (relname)
WHERE relname ~ '^(event|wl)_[0-9]+' ORDER BY age(relfrozenxid) DESC LIMIT 20;
```

After a run the vacuumed tables show a single-digit age and fresh
`last_vacuum` / `last_analyze` timestamps.
