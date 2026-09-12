# Event Tracker 集群化改造方案（草案，2026-09-12）

目标：一个写入端、多个只读 API 端、一套数据库；cloud 与 web 两组 API 在同一实例上
并存，cloud 组加 token 鉴权，web 组新增按精确 UID 查房。

## 1. 现状与问题

两套完全相同的 tracker 各自采集、各自落库，唯一差别是一个开关：

| | CN02 `haruki-toolbox-event-tracker` | VM105 `haruki-event-tracker` |
|---|---|---|
| `privacy.uid_anonymization.enabled` | `true`（web 用 unique_id） | `false`（cloud 用原始 UID） |
| 数据库 | `haruki_tracker_*` 7.65 GB，事件 170-179 | `haruki_event_*` 10.2 GB，事件 164-179 |
| 消费方 | Toolbox 前端经 Oathkeeper `/event-tracker/*` | Haruki-Cloud `tracker.base_url` |

分成两套的根因在代码里：`api::extract::prepare_user_id_mode` 由全局匿名化开关决定
`PublicUserIdMode`，cloud 与 web 的处理函数都走它，所以同一实例无法同时对 bot 给原始
UID、对网页给 unique_id。这也是"以开关区分 cloud/web"的来源，代价是双倍轮询与双倍存储。

## 2. 目标拓扑

```
CN05 (writer)                       CN08 (primary + reader)             CN02 (replica + reader)
┌────────────────────┐   写入(5432)  ┌──────────────────────────┐  流复制  ┌──────────────────────────┐
│ event-tracker      │──────────────▶│ haruki-tracker-postgres  │─────────▶│ haruki-tracker-replica   │
│  role: writer      │               │  (PG18 primary, slot)    │          │  (PG18 hot standby, RO)  │
│  5 区采集 → 远端库  │               ├──────────────────────────┤          ├──────────────────────────┤
│  /internal/updates │◀──WS 订阅─────│ event-tracker role:reader│          │ event-tracker role:reader│
│  (更新流, token)   │◀──WS 订阅─────┼──────────────────────────┼──────────│  读本机 replica          │
└────────────────────┘               │  读本机 primary          │          │  redis: toolbox redis    │
                                     │  redis: 本机新起          │          │  出口: Oathkeeper(web)   │
                                     │  出口: Haruki-Cloud(cloud)│          └──────────────────────────┘
                                     └──────────────────────────┘
```

- **CN05 只写**：五个区的采集 daemon 全开，DSN 指向 CN08 主库；Redis 用 CN05 本机
  （只存 `rank_state`/`ended` 与 border 指纹）。HTTP 只暴露 `/livez`、`/readyz` 和
  `/internal/updates`，不挂 cloud/web 路由。
- **CN08**：新起 PostgreSQL 18 主库（`wal_level=replica`，为 CN02 建物理复制槽）+ Redis +
  reader tracker。Haruki-Cloud 指向 CN08 reader。
- **CN02**：新起第二个 PostgreSQL 实例做热备（现有 toolbox PG 不能整库变只读），reader
  tracker 读本机热备。Oathkeeper 上游名 `haruki-toolbox-event-tracker:8777` 不变。
- **VM105 tracker 与 CN02 旧 `haruki_tracker_*`**：切换后冻结，观察期后删除。

CN05 不承载数据库的原因见前一轮分析：1.6 GB 内存、26 GB 磁盘、且是唯一的游戏 API
上游，复制槽积压 WAL 会把它写满。实测 tailnet 全部直连：CN05→CN08 29 ms、CN05→CN02
12 ms、CN02↔CN08 26 ms。

### 为什么读端各自读本机副本而不是都连 CN08

Toolbox 对 Cloud 板块不产生运行时依赖：复制中断只影响 CN02 数据的新鲜度，不影响可用性。
若两个 reader 都直连 CN08 主库，实现最简单，但 CN02 的 web 查询会随 CN08 一起不可用。

### 复制方式

物理流复制。tracker 每个赛事在运行时动态建四张表（`create_event_tables`），逻辑复制不
复制 DDL，每开一个赛事就要在每个订阅端手工建表并刷新订阅，不可接受。

## 3. Tracker 代码改动（v4.0 线）

### 3.1 `cluster` 配置段与角色

```yaml
cluster:
  role: standalone        # standalone | writer | reader
  token: ""               # /internal/* 的 bearer；reader 订阅 writer 时携带
  writer_url: ""          # reader 必填，例如 http://100.76.159.97:8777
  replica_wait_ms: 1500   # reader 收到更新后等待本机回放到该 LSN 的上限；0 关闭
```

- `standalone`：现行为，不变。
- `writer`：要求每个启用区 `tracker.enabled: true`；不挂 cloud/web 路由；缓存失效不再写
  本机 Redis epoch，而是向 `/internal/updates` 的订阅者广播。
- `reader`：强制 `tracker.enabled: false`；启动时起一个订阅任务连接 writer；**绝不执行
  DDL**（见 3.4）。

### 3.2 实时更新流（需求 2）

reader 主动拨号 writer 的 `GET /internal/updates`（WebSocket，`Authorization: Bearer`），
而不是 writer 向配置好的 peer 列表推 webhook：加减 reader 无需改 writer 配置，断线重连
的责任在 reader，writer 不用维护投递队列。

消息：

```json
{"type":"hello","seq":1234}
{"type":"updated","seq":1235,"server":"cn","eventId":179,"timestamp":1757650000,"lsn":"5/9A3F2C10"}
{"type":"ping"}
```

writer 侧接入点：`EventTrackerBase` 现在直接持有 `api_cache_redis: Option<ConnectionManager>`
并在写入前后调 `begin/finish/abort_event_update`。改为注入一个 `UpdateSink`：

- standalone：原逻辑（本机 Redis epoch + `RealtimeHub`）。
- writer：事务提交后取 `pg_current_wal_insert_lsn()`（一条极轻查询），连同 server/event/
  timestamp 发到广播 channel；`begin`/`abort` 在 writer 模式下是空操作（远端 reader 看不
  到 dirty 标记，也不需要——只在提交后失效即可）。

reader 侧处理 `updated`：

1. 若 `replica_wait_ms > 0`，对该区 engine 轮询 `pg_last_wal_replay_lsn() >= lsn`，最多等
   `replica_wait_ms`（CN08 reader 读的就是主库，配置为 0）。
2. 对本机 api_cache Redis 调 `finish_event_update`（bump epoch）。
3. `RealtimeHub::notify_update`，浏览器 WS 订阅者收到 `updated`。

重连或 `seq` 出现跳变时，reader 对所有已知活动赛事各 bump 一次 epoch（宁可多失效）。
流断开时 `/readyz` 附带 `"updates": "disconnected"` 但不返回 503：TTL（latest 1 s、默认
2 s）仍然兜底新鲜度，只是 WS 推送和亚秒级失效暂停。

### 3.3 cloud/web 同实例并存 + cloud token（需求 3）

- **匿名 uid 强制开启**：`privacy.uid_anonymization.enabled` 在新拓扑下必须为 `true`，
  writer 启动时拒绝关闭状态；`batch_upsert_event_users` 已在写入时为每个用户算好
  `unique_id`，所以库里每个人都有匿名 id，reader 不需要 backfill。
- **web 默认匿名**：web 链固定 `PublicUserIdMode::Unique`，与今天 CN02 的行为一致。
- **显式用真实 uid 查询时回显真实 uid**：仅限调用方在请求里明确给出原始 UID 的入口
  （3.5 的 check-room，以及 `details/user/{id}?idType=uid`）。响应中**被查者本人**的
  `userId` 为原始 UID 并附 `uniqueId`；相邻名次、trace 里出现的其他玩家仍是 unique_id。
- **cloud 保持原行为**：新增 `prepare_cloud_user_id_mode` 恒返回 `Raw`，cloud 处理链
  （`service/cloud.rs`、`snapshot.rs`、`trace.rs`、`round_metrics.rs`）改走它，不再受
  匿名化开关影响。请求参数、响应字段与今天 VM105 上的 cloud 部署完全一致。
- 缓存键已由 `cache_prefix`（`cloud:v2` / web 前缀）区分，两组结果不会串。cloud trace 的
  缓存键目前直接拼 `subject`（原始 UID），改为拼 `sha256(subject)` 前 16 位，避免原始
  UID 进入 Redis 键空间。
- 新配置：

  ```yaml
  cloud_api:
    tokens: []            # 非空即强制；空列表保持开放并在启动时 warn（兼容 standalone 老部署）
  ```

  `cloud_v2_routes()` 挂 `require_cloud_token` 中间件，`Authorization: Bearer <t>`，常量时间
  比较，失败返回 401 JSON。
- Haruki-Cloud：`config.TrackerConfig` 加 `Token`（`HARUKI_TRACKER_TOKEN`），
  `client_tracker.go::getRaw` 的 `SetHeader` 处加 `Authorization`。除 Haruki-Cloud 外没有
  其它 cloud 组消费者（Toolbox-Backend / SekaiColo / sekai-backend 均未引用）。

### 3.4 reader 不执行 DDL

`db::privacy::ensure_column` 现在是"先 ALTER、吃掉 duplicate column 错误"。热备上 ALTER
会报 `cannot execute ALTER TABLE in a read-only transaction`，不是 duplicate 错误，
`prepare_user_id_mode` 会把每个 web 请求变成 500。改法：

- `ensure_column` 先查 `information_schema.columns`，缺列才 ALTER（所有角色受益，避免每
  个赛事首个请求都触发一次报错型 ALTER）。
- reader 角色：缺列直接返回错误（说明 writer 尚未迁移），不 ALTER、不 backfill。

`create_event_tables` 与 `batch_get_or_create_time_ids` 只在采集路径，reader 天然不触达。

### 3.5 web 精确 UID 查房（需求 3）

```
GET /api/v2/web/events/{server}/{event_id}/leaderboards/total/check-room?userId=<原始UID>
GET /api/v2/web/events/{server}/{event_id}/leaderboards/world-bloom/{character_id}/check-room?userId=<原始UID>
GET /api/v2/web/events/{server}/{event_id}/leaderboards/total/details/user/{id}?idType=uid
```

- 原始 UID → `event_<id>_users.user_id` 单点查找 → `user_id_key` → 当前名次，然后复用
  web 的 rank detail 构造（含相邻名次）。
- 被查者本人：`userId` 回显原始 UID，另附 `uniqueId`；相邻玩家与 trace 中的其他玩家仍
  只有 unique_id。未被追踪返回 404。
- 缓存键用解析后的 unique_id 加 `raw=1` 标记，原始 UID 不进缓存键；access log 的 query
  串需确认是否脱敏。
- Oathkeeper 公开规则 `haruki-public-tracker-web-v2-prefixed` 的路径白名单要加
  `check-room`；与其它公开接口一样不要求登录。

### 3.6 其它

- `/readyz` 在 reader 角色附带复制延迟：`SELECT now() - pg_last_xact_replay_timestamp()`。
- 版本：`v4.0.0`，配置示例与 `CLAUDE.md`、`WEB_API_CAPABILITIES.md` 同步。

## 4. 基础设施与迁移

### 4.1 CN08

- `haruki-tracker-postgres`（postgres:18-alpine，`mem_limit 2g`，`shared_buffers=512MB`，
  `wal_level=replica`，`max_wal_senders=4`，`max_slot_wal_keep_size=20GB`），端口
  `127.0.0.1:5432` + `100.125.86.24:5432`；`pg_hba` 只放行 CN05 与 CN02 的 tailnet 地址。
  `max_slot_wal_keep_size` 是必须的：CN02 热备长时间离线时主库丢弃复制槽而不是把 53 GB
  写满（按 VM105 实测 9.3 GB/天 WAL，不设上限约 5 天写满）。
- `haruki-tracker-redis`（`127.0.0.1` only，`mem_limit 128m`）。
- `haruki-event-tracker`（reader，`127.0.0.1:8777` + `100.125.86.24:8777`）。
- 镜像经 mihomo 拉取（CN08 已有）。

### 4.2 CN02

- `haruki-tracker-replica`（postgres:18-alpine，`pg_basebackup -R` 自 CN08，`hot_standby=on`），
  只发布 `127.0.0.1:5433`。磁盘：当前可用 16 GB，热备初始 10.2 GB，删除旧
  `haruki_tracker_*` 后回收 7.65 GB；每个五区赛事周期增长约 0.8 GB，需要在几个月内定
  保留策略（按赛事删旧表）或扩盘。
- 现有 `haruki-toolbox-event-tracker` 改为 reader 配置，DSN 指向本机热备。

### 4.3 CN05

- `haruki-event-tracker`（writer，`127.0.0.1:8777` + `100.76.159.97:8777`），DSN 指向
  CN08，Redis 用本机 `redis`，`sekai_api.api_endpoint` 走 compose 服务名回环。
- 内存：VM105 上同配置的 tracker 实测 RSS 需先量一次（`docker stats`），CN05 可用 ~860 MB。

### 4.4 数据迁移与切换

1. 以 VM105 `haruki_event_*` 为基准（比 CN02 多 164-169 五个赛事）。历史赛事先
   `pg_dump -Fc` 经 CN05 中转导入 CN08（VM105↔CN08 无直连，经 CN05 两跳都是直连）。
2. CN02 `pg_basebackup` 自 CN08，起热备，验证 `pg_stat_replication`。
3. 切换窗口：停 VM105 tracker → 只导当前赛事的四张表增量 → 起 CN05 writer → 起两个
   reader → Haruki-Cloud 改 `tracker.base_url` + token → Oathkeeper 规则加 check-room 并重启。
   采集空窗约等于当前赛事表导入时间（分钟级）。writer 首个 tick 因 Redis 无 rank_state
   会把全部名次当作变化写一次全量快照，属预期。
4. 观察 24 h：writer 日志、两端 `/readyz`、复制延迟、Kuma（46 号改指 CN08 reader，新增
   CN02 reader、writer、复制延迟三个监控）。
5. 冻结 VM105 tracker 与 CN02 旧库，一周后删除。

## 5. 风险与边界

- writer 仍是采集单点（与今天相同）。CN05↔CN08 掉到 DERP 时写入变慢但不丢：
  `flush_interval_secs: 15` 攒批，cron 的 `try_lock` 跳过堆积的 tick。
- 每次 flush 事务跨 29 ms 链路做若干次往返，估算 100~200 ms，秒级采集下可接受；
  `flush_hot_ranks: 10` 的即时落库同样受此延迟。
- 热备上的查询与 WAL 回放冲突可能被取消（`hot_standby_feedback=on` 缓解，代价是主库
  膨胀受热备长查询牵制；trace 查询有并发上限，可接受）。
- Oathkeeper 规则不热加载，改完必须重启 `haruki-toolbox-services-oathkeeper-1`。

## 6. 已定决策（2026-09-12）

1. 主库放 CN08，CN02 起第二个 PG 做热备。
2. 更新流由 reader 拨号 writer。
3. web 精确 UID 查房公开，不要求登录；被查者回显原始 UID，其他人匿名。
4. cloud token 用 `tokens` 列表，每个消费方一枚。
5. 匿名 uid 强制开启；web 默认匿名，显式真实 uid 查询才回显；cloud 行为不变。

## 7. 实施状态（2026-09-12）

- [x] Tracker 代码：`cluster` 角色（standalone/writer/reader）、`/internal/updates` 更新流与
      reader 订阅、`cloud_api.tokens` bearer 鉴权、cloud 恒原始 UID / web 恒 unique_id、
      reader 只读保护（`DatabaseEngine::is_read_only`，缺列只校验不 ALTER）、
      web `check-room?userId=` 与 `details/user/{uid}?idType=uid`、`/readyz` 附带角色/链路/复制延迟。
      分支 `feat/cluster-roles`，178 个单元测试通过（含 writer 流与 reader 订阅端到端）。
- [x] Haruki-Cloud：`tracker.token` / `HARUKI_TRACKER_TOKEN` → `Authorization: Bearer`。
      分支 `feat/tracker-token`。
- [ ] 发版 `v4.0.0`、Haruki-Cloud 发版。
- [ ] 基础设施：CN08 主库 + Redis + reader；CN02 热备 + reader；CN05 writer。
- [ ] 数据迁移与切换、Oathkeeper 规则加 `check-room`、Kuma 监控、skill 文档。
- [ ] 清理旧库与 VM105 tracker。

## 8. 实施顺序

1. Tracker 代码：角色/更新流/cloud token/reader DDL 保护/web check-room，发 `v4.0.0`。
2. Haruki-Cloud 加 token 支持并发版。
3. CN08 主库 + Redis + reader；CN02 热备 + reader 改配；CN05 writer。
4. 迁移与切换、监控、文档（cluster-ops skill 的 migrations 记录）。
5. 清理旧库与 VM105 tracker。
