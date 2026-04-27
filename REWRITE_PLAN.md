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
- ⚠ `serde_yaml 0.9.34+deprecated`:dtolnay 已停止维护,但 API 稳定。Phase 1 暂留,后续如出现问题再切 `serde_yml`(API 兼容 fork)

### Phase 1 — 基础数据层 `[x]`
- [x] `config.rs`:Config / BackendConfig / RedisConfig / SekaiAPIConfig / ServerConfig 结构体 + `load_from_file`(`gorm_config` YAML 键保留以兼容旧配置)
- [x] `model/enums.rs`:SekaiServerRegion / EventType / EventStatus / WorldBloomType / SpeedType / Unit + `SEKAI_EVENT_RANKING_LINES_{NORMAL,WORLD_BLOOM}` 常量
- [x] `model/sekai.rs`:Top100Response / BorderResponse / PlayerRankingSchema / UserWorldBloomChapterRanking 等(receive-only,丢弃 Go 版未消费的字段)
- [x] `model/api.rs`:UserLatestRankingQueryResponse / UserAllRankingDataQueryResponse / EventStatusResponse 等;`RecordedRankData` 用 `#[serde(untagged)]` enum 替代 Go 的 `interface{}`
- [x] `model/tracker.rs`:PlayerState / RankState 保留短键(`s`/`r`/`u`)字节兼容 Go Redis state;新增 WorldBloomKey / HandledRankingData
- [x] `model/db_config.rs`:DbConfig / NamingConfig / LoggerConfig(对齐 Go `GormConfig` 字段)
- [x] `model/event.rs`:EventStatus / Event / WorldBloom / WorldBloomChapterStatus,master data 解析输出
- [x] `logger.rs`:tracing-subscriber + `Mutex<File>` 文件 mirror,自定义 `GoStyleFormat` 保留 `[ts][LEVEL][target] msg` 格式
- 通过 `cargo check --all-targets`,无 warning

### Phase 2 — 数据库层 `[x]`
- [x] `db/entity/{time_id,event_users,event,world_bloom}.rs`:4 个 SeaORM Entity(动态表名:`Entity { table_name: &'static str }` + 手写 `EntityName`)
- [x] `db/table_name.rs`:`TableKind` 枚举 + `intern(kind, event_id)` `Box::leak` 缓存,4 种表名格式与 Go 版一致(单测覆盖)
- [x] `db/schema.rs`:`create_event_tables`,`Schema::create_table_from_entity` + `if_not_exists()`,World Bloom 分支
- [x] `db/engine.rs`:多方言(MySQL/Postgres/Sqlite),连接池默认 100/10/3600s,`parse_simple_duration` Go 风格 duration(单测)
- [x] `db/query/ranking.rs`:`fetch_latest_ranking` / `_by_rank` / `fetch_all_rankings` / `_by_rank`(三表 JOIN 共用 `ranking_select`)
- [x] `db/query/world_bloom.rs`:四个 World Bloom 查询变体(`wl_<id>` JOIN,带 `character_id` 过滤)
- [x] `db/query/lines.rs`:`fetch_ranking_lines` / `fetch_world_bloom_ranking_lines`,每个 rank 一个并行 `find_by_statement` 通过 `futures::future::join_all`,失败静默丢弃
- [x] `db/query/growth.rs`:`fetch_ranking_score_growths` / WB 版本,并行 + `(latest - earliest)` 增长
- [x] `db/query/user.rs`:`get_user_data`
- [x] `db/query/heartbeat.rs`:`write_heartbeat` / `fetch_latest_heartbeat`(共享 `batch_get_or_create_time_ids`)
- [x] `db/query/batch.rs`:`batch_insert_event_rankings` / `batch_insert_world_bloom_rankings` + `batch_get_or_create_{time_ids,user_id_keys}`(`OnConflict::do_nothing` 兜底重试)
- 通过 `cargo check --all-targets` 与 `cargo test --lib`(4 passed)

### Phase 3 — Sekai API client `[x]`
- [x] `sekai_api/client.rs`:`HarukiSekaiAPIClient { reqwest::Client, api_endpoint }`,User-Agent `Haruki-Event-Tracker/{CARGO_PKG_VERSION}`,可选 `X-Haruki-Sekai-Token` 头,20s 超时
- [x] `sekai_api/endpoint.rs`:`get_top100` / `get_border`,后者返回 `([u8; 32], BorderRankingResponse)` SHA-256 hash + sonic-rs 解析
- [x] `sekai_api/error.rs`:`SekaiApiError { Request, Status, Decode }` 三态(thiserror)
- [x] `#[tracing::instrument(skip(self), fields(server, event_id))]` 覆盖
- 新增 `sha2 = "0.10"` 依赖

### Phase 4 — 事件解析器 `[x]`
- [x] `tracker/parser.rs`:`EventDataParser { server, master_dir }`,`tokio::fs::read` + `sonic_rs::from_slice` 读取 `events.json` / `worldBlooms.json`(Go 版 dead-code `LoadData(path)` 通用缓存已废弃)
- [x] `get_current_event_status` 输出 `Option<EventStatus>`,带 `ChapterStatuses`(World Bloom 类型才填),非 World Bloom 走空 HashMap
- [x] `get_world_bloom_character_statuses`:跳过 `Finale` chapter,按 `(start, aggregate, end)` 计算每角色 chapter 状态
- [x] `event_time_remain` 复刻 Go 版多语言 remaining-time 格式化(JP/CN/TW/EN/KR);5 个单测覆盖
- [x] `ParseError { Read, Parse }` thiserror 枚举,`#[tracing::instrument]` 覆盖入口

### Phase 5 — Tracker 核心 `[x]`
- [x] `tracker/diff.rs`:`diff_rank_based` + `merge_rankings` + `extract_world_bloom_rankings` + `build_event_records` + `build_world_bloom_rows`(**纯函数**,11 个单测)
- [x] `tracker/cache.rs`:`detect_cache`(SHA-256 hex,与 Go `fmt.Sprintf("%x", hash)` 字节一致)
- [x] `tracker/state.rs`:`load_rank_state` / `save_rank_state` / `check_event_ended_flag` / `set_event_ended_flag`,key schema 与 Go 版字节一致;Go 版死代码 `user_state` 不移植
- [x] `tracker/base.rs`:`EventTrackerBase` 状态机(`init` / `record_ranking_data` / `handle_ranking_data` / `set_event_ended` / WB chapter setters);Go 版死代码 `prevEventState` / `prevUserState` / `lastUpdateTime` / `getFilterFunc` 不移植
- [x] `tracker/daemon.rs`:`HarukiEventTracker` 编排,`track_ranking_data` 入口,新事件自动切换;WB chapter 状态变化逐章 finalize
- 决策:Go 版 `tracker/world_bloom.rs` 拆分文件不复刻——WB 助手已落在 `diff.rs`,daemon 编排在 `daemon.rs`,空文件已删除

### Phase 6 — HTTP API 层 `[x]`
- [x] `api/state.rs`:`AppState { Arc<Inner { dbs: HashMap<SekaiServerRegion, Arc<DatabaseEngine>> }> }`
- [x] `api/json.rs`:sonic-rs 实现 `IntoResponse`(全部 GET 路由,无需 `FromRequest`)
- [x] `api/error.rs`:`ApiError { InvalidServer, NotFound, BadRequest, Db(#[from] DbErr) }` → `{"error": "..."}` JSON,400/404/500 映射
- [x] `api/extract.rs`:`resolve_engine(&AppState, &str)` 帮助函数,Path 元组直接用 axum 内置
- [x] `api/access_log.rs`:`axum::middleware::from_fn` Tower 中间件,format `YYYY/MM/DD HH:MM:SS | status | latency | ip | METHOD path`,IP 优先 `X-Forwarded-For` / `X-Real-IP` 否则 `ConnectInfo<SocketAddr>`
- [x] `api/handler/{ranking,trace,world_bloom,user,lines,status}.rs`:14 个路由全量移植,latest/trace 共用 404 语义(rank 查询无行 404,user 查询 ranking+user 双空才 404)
- [x] `api/router.rs`:`/event/{server}/{event_id}` 子路由,挂载 CompressionLayer(gzip+br) → access log → CatchPanicLayer

### Phase 7 — main / 调度 / 关停 `[x]`
- [x] `main.rs`:`#[tokio::main]`,config → logger → `app::build` → axum `serve().with_graceful_shutdown` → `shutdown::run`;SSL 配置打 warn 并退到 HTTP(由反代终结 TLS),后续阶段再补
- [x] `app.rs`:`AppContext { state, dbs, trackers, scheduler }`;`build` 顺序:Redis ConnectionManager → Sekai API client → 每个 enabled server 建 DB engine → 建 daemon `init` 失败仅 warn(让首次 tick 重试)→ `JobScheduler::new().add(Job::new_async(cron, |_| daemon.lock().await.track_ranking_data()))` → `scheduler.start()`
- [x] cron 兼容:`use_second_level_cron: false` 时给 5 段表达式补 `0 ` 前缀变 6 段(tokio-cron-scheduler 强制 6 段),保持旧 YAML 不动
- [x] `shutdown.rs`:`signal()` 监听 SIGINT/SIGTERM(unix)或 Ctrl+C(windows);`run()` 顺序 scheduler.shutdown → drop(trackers,顺带丢 Redis ConnectionManager 句柄)→ 逐个 `Arc::try_unwrap(engine).close()`

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
