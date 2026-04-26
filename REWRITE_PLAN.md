# Rust Rewrite Plan

将 Haruki Event Tracker 从 Go 重写为 Rust。本文档记录技术决策、模块结构与各阶段进度。

- 起始日期:2026-04-26
- 工作分支:`rewrite/rust`
- 行为目标:与 Go 版**严格对等**,Redis 状态与数据库 schema 字节兼容,以支持按服务器灰度切换

---

## 1. 技术栈

| 组件 | 选型 | 备注 |
|---|---|---|
| Runtime | Tokio (full) | edition 2024,MSRV 1.85(edition 2024 要求 1.85+) |
| HTTP | Axum 0.8 + tower-http | compression / trace / catch-panic |
| ORM | SeaORM + sea-query | sea-query 用于动态表名 |
| JSON | sonic-rs | 自定义 `Json<T>` 替代 `axum::Json` |
| HTTP Client | reqwest (rustls) | 关闭默认 json feature,走 sonic-rs |
| Redis | `redis` (tokio-comp, connection-manager) | 不上 fred |
| 调度 | tokio-cron-scheduler | 5/6 段表达式由 `use_second_level_cron` 决定 |
| 日志 | tracing + tracing-subscriber | `#[instrument]` + `server`/`event_id` span 字段 |
| 配置 | serde + serde_yaml | |
| 错误 | thiserror | 模块级错误枚举 |

## 2. 关键决定

- **动态表名**:Entity 静态定义,查询/建表时通过 sea-query 的 `TableRef::table_alias` 覆写真实表名。`db/table_name.rs` 作为唯一拼接点。
- **Redis 兼容**:key schema `haruki:tracker:<server>:<event>:{rank_state,user_state,ended}` 与 JSON value 形状必须与 Go 版一致(见 `tracker/trackerbase.go:88-166`)。
- **模块文件规范**:全程使用 `foo.rs` + 同层 `foo/` 目录,**不使用 `mod.rs`**。
- **可观测性**:在 handler / tracker tick / sekai_api 端点 / 关键 db query 加 `#[tracing::instrument]`,span 统一带 `server`、`event_id`。
- **测试策略**:Go 版无测试;Rust 版至少为 `tracker::diff` (`diff_rank_based` / `merge_rankings`) 补 unit test,后续视情况扩展到 query 层。

## 3. 模块结构

```
src/
├── main.rs                  # 启动序列 + signal handling(薄)
├── lib.rs                   # 顶层模块重导出,便于 integration test
│
├── config.rs                # Config / *Config struct + load_from_file
│
├── logger.rs                # tracing-subscriber 初始化 + 文件 multi-writer
│
├── model.rs
├── model/
│   ├── enums.rs             # SekaiServerRegion / EventType / EventStatus / RankingLines 常量
│   ├── sekai.rs             # Sekai API 请求/响应 schema
│   ├── api.rs               # 对外 HTTP 响应 schema
│   ├── tracker.rs           # PlayerState / RankState / WorldBloomKey / HandledRankingData
│   └── db_config.rs         # DbConfig / NamingConfig / LoggerConfig
│
├── db.rs
├── db/
│   ├── engine.rs            # 多方言 DatabaseEngine,连接池,Close/Ping
│   ├── table_name.rs        # 表名拼接的单一事实源
│   ├── schema.rs            # create_event_tables(Schema + 表名覆写)
│   ├── entity.rs
│   ├── entity/
│   │   ├── time_id.rs
│   │   ├── event_users.rs
│   │   ├── event.rs
│   │   └── world_bloom.rs
│   ├── query.rs
│   └── query/
│       ├── ranking.rs       # FetchLatest{,ByRank} / FetchAll{,ByRank}
│       ├── world_bloom.rs   # World Bloom 四个查询变体
│       ├── lines.rs         # FetchRankingLines / WorldBloom 版本
│       ├── growth.rs        # FetchRankingScoreGrowths / WorldBloom 版本
│       ├── user.rs          # GetUserData
│       ├── heartbeat.rs     # WriteHeartbeat / FetchLatestHeartbeat
│       └── batch.rs         # BatchInsertEventRankings / WorldBloom 版本
│
├── sekai_api.rs
├── sekai_api/
│   ├── client.rs            # HarukiSekaiAPIClient (reqwest)
│   ├── endpoint.rs          # get_top100 / get_border (+ body hash)
│   └── error.rs             # SekaiApiError
│
├── tracker.rs
├── tracker/
│   ├── daemon.rs            # HarukiEventTracker:对外编排,scheduler 调它
│   ├── base.rs              # EventTrackerBase:状态机本体
│   ├── parser.rs            # EventDataParser:master data 读取
│   ├── diff.rs              # diff_rank_based / merge_rankings(纯函数 + 单测)
│   ├── cache.rs             # detect_cache(Redis hash 比对)
│   ├── state.rs             # Redis 状态加载/保存,ended flag
│   └── world_bloom.rs       # handle_world_bloom + processWorldBloomChapter 逻辑
│
├── api.rs
├── api/
│   ├── state.rs             # AppState (Arc 内置)
│   ├── router.rs            # Router 构建 + 中间件挂载
│   ├── error.rs             # ApiError → IntoResponse
│   ├── extract.rs           # CommonParams extractor
│   ├── json.rs              # sonic-rs Json<T>:FromRequest + IntoResponse
│   ├── access_log.rs        # 自定义 Tower layer
│   ├── handler.rs
│   └── handler/
│       ├── ranking.rs       # latest-ranking/{user,rank}
│       ├── trace.rs         # trace-ranking/{user,rank}
│       ├── world_bloom.rs   # latest/trace world-bloom
│       ├── lines.rs         # ranking-lines + score-growth
│       ├── user.rs          # user-data
│       └── status.rs        # /status
│
└── shutdown.rs              # 关停协调器:axum→scheduler→trackers→redis→db
```

## 4. 阶段进度

图例:`[ ]` 未开始 / `[~]` 进行中 / `[x]` 完成

### Phase 0 — 项目骨架与依赖 `[x]`
- [x] crate 名定为 `haruki-event-tracker`(对齐二进制名,kebab-case);lib 名 `haruki_event_tracker`
- [x] `Cargo.toml`:edition 2024,rust-version 1.85(edition 2024 强制要求)
- [x] 建立 `src/` 目录骨架,53 个 `.rs` 文件,`foo.rs` + `foo/` 同层模式贯穿
- [x] 通过 `cargo check`(29.93s,无 warning)
- 已解析依赖版本:axum 0.8.9 / sea-orm 1.1.20 / sea-query 0.32.7 / sonic-rs 0.3.17 / redis 0.27.6 / tokio-cron-scheduler 0.13.0 / reqwest 0.12.28 / tower-http 0.6.8
- ⚠ `serde_yaml 0.9.34+deprecated`:dtolnay 已停止维护,Phase 1 切到 `serde_yml`(社区维护 fork,API 兼容)

### Phase 1 — 基础数据层 `[ ]`
- [ ] `config.rs`:Config / BackendConfig / RedisConfig / SekaiAPIConfig / ServerConfig 结构体 + `load_from_file`
- [ ] `model/enums.rs`:SekaiServerRegion / EventType / EventStatus / WorldBloomType / SpeedType / Unit + `RankingLinesNormal/WorldBloom` 常量
- [ ] `model/sekai.rs`:Top100Response / BorderResponse / PlayerRankingSchema / UserWorldBloomChapterRanking 等
- [ ] `model/api.rs`:UserLatestRankingQueryResponse / UserAllRankingDataQueryResponse / EventStatusResponse 等
- [ ] `model/tracker.rs`:PlayerState / RankState / WorldBloomKey / HandledRankingData
- [ ] `model/db_config.rs`:DbConfig / NamingConfig / LoggerConfig
- [ ] `logger.rs`:tracing-subscriber + 文件 multi-writer,保留 `[Component] LEVEL` 格式

### Phase 2 — 数据库层 `[ ]`
- [ ] `db/entity/{time_id,event_users,event,world_bloom}.rs`:4 个 SeaORM Entity
- [ ] `db/table_name.rs`:`build_table_name(server, event_id, kind)` 单一事实源
- [ ] `db/schema.rs`:`create_event_tables`(Schema + 表名覆写,含 isWorldBloom 分支)
- [ ] `db/engine.rs`:多方言 DatabaseEngine,连接池,Close/Ping
- [ ] `db/query/ranking.rs`:FetchLatestRanking / ByRank / FetchAllRankings / ByRank
- [ ] `db/query/world_bloom.rs`:World Bloom 四个查询变体
- [ ] `db/query/lines.rs`:FetchRankingLines / FetchWorldBloomRankingLines
- [ ] `db/query/growth.rs`:FetchRankingScoreGrowths / WorldBloom 版本
- [ ] `db/query/user.rs`:GetUserData
- [ ] `db/query/heartbeat.rs`:WriteHeartbeat / FetchLatestHeartbeat
- [ ] `db/query/batch.rs`:BatchInsertEventRankings / BatchInsertWorldBloomRankings

### Phase 3 — Sekai API client `[ ]`
- [ ] `sekai_api/client.rs`:HarukiSekaiAPIClient(reqwest 实例 + base url + token)
- [ ] `sekai_api/endpoint.rs`:`get_top100`、`get_border`(返回 `[u8; 32]` hash)
- [ ] `sekai_api/error.rs`:SekaiApiError(thiserror)
- [ ] `#[tracing::instrument]` 覆盖

### Phase 4 — 事件解析器 `[ ]`
- [ ] `tracker/parser.rs`:`tokio::fs` + sonic-rs 读取 `events.json` / `worldBlooms.json` / `eventCards.json` 等
- [ ] 输出 `EventStatus { event_id, event_type, event_status, chapter_statuses }`
- [ ] 与 Go 版 `eventparser.go` 行为对照

### Phase 5 — Tracker 核心 `[ ]`
- [ ] `tracker/diff.rs`:`diff_rank_based` + `merge_rankings`(**纯函数**)
- [ ] `tracker/diff.rs` 单测:典型路径 / rank 重复 / 空数据 / cache hit 旁路
- [ ] `tracker/cache.rs`:`detect_cache`(Redis hash 比对)
- [ ] `tracker/state.rs`:`load_state_from_redis` / `save_state_to_redis` / ended flag,**key schema 与 Go 版字节一致**
- [ ] `tracker/world_bloom.rs`:`handle_world_bloom` + `processWorldBloomChapter`
- [ ] `tracker/base.rs`:EventTrackerBase 状态机
- [ ] `tracker/daemon.rs`:HarukiEventTracker 编排,新事件自动切换

### Phase 6 — HTTP API 层 `[ ]`
- [ ] `api/state.rs`:AppState(`Arc<Inner>`)
- [ ] `api/json.rs`:sonic-rs `Json<T>` 同时实现 `FromRequest` + `IntoResponse`
- [ ] `api/error.rs`:ApiError → IntoResponse
- [ ] `api/extract.rs`:CommonParams extractor
- [ ] `api/access_log.rs`:Tower layer,对齐 fiber `${time} | ${status} | ${latency} | ${ip} | ${method} ${path}` 格式
- [ ] `api/handler/{ranking,trace,world_bloom,lines,user,status}.rs`:14 个路由全量移植
- [ ] `api/router.rs`:挂载 CompressionLayer、CatchPanicLayer、access log

### Phase 7 — main / 调度 / 关停 `[ ]`
- [ ] `main.rs`:`#[tokio::main]`,启动序列 config → logger → redis → sekai_api → 各 server → scheduler → axum
- [ ] tokio-cron-scheduler 注册任务,`use_second_level_cron` 切换 5/6 段
- [ ] `tokio::signal` 监听 SIGINT/SIGTERM
- [ ] `shutdown.rs`:有序关停 axum → scheduler → trackers → redis → db

### Phase 8 — CI / Docker / 切换 `[ ]`
- [ ] `Dockerfile`:`rust:1.84-alpine` 多阶段构建
- [ ] `.github/workflows/release.yml`:替换 setup-go → setup-rust + cross,矩阵保留四目标
- [ ] `.github/workflows/docker.yml`:无需大改,确认构建参数
- [ ] 删除 Go 源:`go.mod` / `go.sum` / `main.go` / `api/` / `tracker/` / `utils/` / `config/`
- [ ] 更新 `README.md` 与 `CLAUDE.md`
- [ ] 灰度计划:按 server 逐个切换,先切流量最小的服务器观察 24h

## 5. 待确认事项

- [ ] crate 名最终拍板
- [ ] Phase 0 是否在 `rewrite/rust` 直接提交,还是再开子分支
- [ ] master data 文件结构是否有变(Phase 4 开始前 sample 一次)
- [ ] 灰度切换时是否需要"双写"过渡期,还是直接停 Go 版起 Rust 版

## 6. 行为对照清单(每阶段验收用)

切换前必须逐项验证:

- [ ] 同一 `(server, event_id)` 下,Rust 版与 Go 版生成的表名完全一致
- [ ] Rust 版能读取 Go 版写入的 Redis 状态并继续追踪,无重复或丢失
- [ ] 14 个 HTTP 端点的响应 JSON 字段名、嵌套结构、空值处理与 Go 版一致
- [ ] heartbeat 在 API 失败时仍写入(status=1)
- [ ] World Bloom 多 chapter 重叠期所有 chapter 都被记录
- [ ] 事件结束自动 finalize 并写 ended flag
