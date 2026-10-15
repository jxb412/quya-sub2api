# Quya Sub2API 项目交接与运维说明

本文档是 `jxb412/quya-sub2api` 的项目交接入口，只记录本项目的目录、自定义
改动、注意事项、更新方式和部署方式。其他代理项目、支付前端、检查副本和临时
会话内容不属于本项目，不在这里记录。

## 仓库信息

| 项目 | 地址或说明 |
|---|---|
| 本地目录 | `D:\btc\st\quya-sub2api` |
| 个人仓库 | `https://github.com/jxb412/quya-sub2api` |
| 上游仓库 | `https://github.com/Wei-Shaw/sub2api` |
| 默认分支 | `main` |
| 发布镜像 | `ghcr.io/jxb412/sub2api:<version>` |
| 当前开发版本 | `0.4.15`（合并上游 `v0.2.16`，已发布，两台正式机已升级） |

`origin` 是个人仓库，`upstream` 是上游仓库。个人改动必须提交到个人仓库，
不能直接把上游分支覆盖到个人 `main`。

## 项目结构

```text
quya-sub2api/
├─ backend/       Go 网关、处理器、服务、数据访问和数据库迁移
├─ frontend/      Vue 3 管理后台和用户页面
├─ deploy/        Docker/systemd 部署、配置和环境模板
├─ docs/          功能、支付、API、运维和安全文档
├─ openspec/      项目设计提案和变更记录
├─ README_CN.md   中文项目入口
├─ DEV_GUIDE.md   开发与同步指南
└─ Makefile       常用命令
```

## 当前自定义功能

- OpenAI 请求在账号调度前按 Responses、Chat Completions、Messages 入口筛选。
- `openai_responses_only` 独立限制 Responses、compact 和 WebSocket 入口。
- `codex_cli_only` 只判断 Codex 客户端身份，不自动等同 Responses-only。
- 调度器、sticky 路由和 failover 会跳过不符合入口条件的账号。
- 风控中心支持按 `pro`、`plus`、`team`、`free` 账号类型单选或多选。
- 内置更新检查和回滚源改为 `jxb412/quya-sub2api`。
- 首次安装和数据库启动初始化默认关闭。

主要代码位置：

- OpenAI 调度：`backend/internal/service/openai_*`
- Codex 转换和身份：`backend/internal/service/openai_codex_*`
- 风控：`backend/internal/service/content_moderation.go`
- 后台风控页面：`frontend/src/views/admin/RiskControlView.vue`
- 更新服务：`backend/internal/service/update_service.go`
- 启动和安装：`backend/cmd/server/main.go`、`backend/internal/setup/`
- 数据库连接和迁移：`backend/internal/repository/ent.go`、`backend/migrations/`

## 默认安全开关

生产 Docker Compose 默认使用：

```dotenv
SETUP_ENABLED=false
AUTO_SETUP=false
DATABASE_INITIALIZATION_ENABLED=false
```

含义：

- 不启动首次安装向导。
- 不自动创建数据库、管理员和配置文件。
- 不在服务启动时执行迁移、JWT 密钥补写或简单模式默认数据写入。
- 业务运行仍会正常读写已有数据库。

真正新安装时，先备份环境文件，再同时设置：

```dotenv
SETUP_ENABLED=true
AUTO_SETUP=true
DATABASE_INITIALIZATION_ENABLED=true
```

安装完成后恢复为 `false`。

### 执行数据库迁移

`DATABASE_INITIALIZATION_ENABLED` 是唯一控制启动期迁移的开关：置为 `true` 时，服务
启动会对目标库执行 `backend/migrations/` 中所有未登记的迁移，并按
`sha256(TrimSpace(内容))` 登记到 `schema_migrations`；置为 `false` 时，启动日志会打印
`database initialization disabled; skipping startup migrations and bootstrap writes`
并跳过全部迁移。生产环境默认保持 `false`，需要执行迁移时用下面任一方式。

方式 A：手工执行 + 登记（本项目生产升级实际使用，可控性最好）

1. 备份 PostgreSQL 与 compose/env。
2. 在维护窗口执行本次新增的迁移 SQL（本项目迁移均为幂等语句）。
3. 按 `sha256(TrimSpace(内容))` 计算校验和，写入
   `schema_migrations(filename, checksum)`。
4. 检查所有连接同一数据库的应用节点。

方式 B：临时开启启动迁移（2026-10-09 已在测试机验证）

已验证结论：对**已安装**的库，把 `DATABASE_INITIALIZATION_ENABLED` 改成 `true` 并重建
容器，启动时会自动重放缺失的迁移、写入 `schema_migrations`（`applied_at` 为本次启动
时间），且不再打印 skip 日志。

注意事项：

- 生产 compose 在 `environment:` 中把该变量写死为 `false`，只改 `.env` 不会生效；
  必须改 compose 后再用 `docker compose up -d` 重建容器。
- 该开关同时放开启动期的 bootstrap 写入（JWT 密钥补写、SIMPLE 模式默认分组与管理员
  并发写入）。迁移完成后要改回 `false` 并重建容器。
- 对**空库**无效：缺少 `/app/data/.installed` 安装锁且 `SETUP_ENABLED=false` 时，进程
  会直接以 1 退出并打印
  `first-run setup is disabled; set SETUP_ENABLED=true for an explicit installation`。
- 多节点共用同一数据库时，迁移由 PostgreSQL advisory lock 串行化；即便如此也要在所有
  节点上同时改回 `false`，避免节点间行为不一致。

## 部署

生产推荐使用 `deploy/docker-compose.local.yml`，数据保存在部署目录下：

```bash
cp deploy/.env.example deploy/.env
# 编辑 deploy/.env，设置数据库密码、JWT_SECRET、TOTP_ENCRYPTION_KEY
cd deploy
docker compose -f docker-compose.local.yml up -d
docker compose -f docker-compose.local.yml ps
docker compose -f docker-compose.local.yml logs --tail=100 sub2api
```

更新镜像前先确认持久化挂载：

```bash
docker inspect sub2api --format '{{range .Mounts}}{{println .Source "->" .Destination}}{{end}}'
```

不要执行 `docker compose down -v`。只更新应用容器：

```bash
docker pull ghcr.io/jxb412/sub2api:<version>
docker compose -f docker-compose.local.yml up -d --no-deps sub2api
```

拉取镜像不会中断业务；替换应用容器可能造成短暂中断，进行中的流式请求可能
断开。服务器操作系统不需要重启。

## 发布与更新

`release.yml` 只在 `v*` 标签上发布稳定版本。CI 通过后创建例如 `v0.4.6`，
会构建二进制、GitHub Release 和：

```text
ghcr.io/jxb412/sub2api:0.4.6
ghcr.io/jxb412/sub2api:latest
```

Docker 服务器使用固定版本标签更容易回滚。内置更新检查适用于二进制/systemd
部署；Docker 部署仍需拉取镜像并重建应用容器。

`0.4.6` 基于本项目 `0.4.5`，合并上游 `v0.2.5`。上游主要变化包括 OpenCode
平台、站点类型开关、订阅/API Key 批量管理、Codex 配额窗口修复、Responses Lite
namespace 修复、OpenAI WebSocket 连接池与执行作用域修复，以及用量费用精度改进。

本次新增两个幂等迁移文件：

- `238_opencode_go_platform.sql`：在现有平台约束中增加 `opencode_go`。
- `238_purge_unlimited_user_platform_quotas.sql`：删除三档额度均为 `NULL` 的无效平台额度行。

生产环境保持 `DATABASE_INITIALIZATION_ENABLED=false` 时，应用不会自行执行这两项
迁移。升级前必须先备份 PostgreSQL，在维护步骤中执行并登记迁移（执行方式见上文
「执行数据库迁移」），再同时替换所有连接同一数据库的应用节点；不需要重启
PostgreSQL。

本次在隔离环境完成了从 `0.4.5` 数据库原地升级到 `0.4.6` 的验证：两项迁移均已
登记，四个相关平台约束包含 `opencode_go`，原管理员数据和登录状态保持正常。

`0.4.13` 基于本项目 `0.4.12`，合并上游 `v0.2.15`（146 个提交）。上游主要变化：新增
Cline、Command Code 平台，平台白名单由数据库 CHECK 约束改为应用层校验；平台清单
统一由 platform profile 驱动转发、探测与账号表单；修复加密 reasoning 签名被拒、
Anthropic thinking 缺少 signature、Chat 与 Responses 互转、WebSocket 长连接计费、
Grok 空流 failover 等问题。

OAuth 历史回放 `web_search_call` 的修复改用官方实现（`dab3b87ea`、`64caa9af8`、
`85e311095`，含 Responses Lite 的 `additional_tools` 变体），不再保留本地早期移植
版本，相关文件与上游逐字节一致。

本次新增一个幂等迁移：

- `242_drop_platform_check_constraints.sql`：删除 `user_platform_quotas_platform_check`
  与 `composite_model_routes_target_platform_check` 两个 CHECK 约束，平台白名单改由
  应用层校验，新增平台不再需要数据库迁移。

生产环境保持 `DATABASE_INITIALIZATION_ENABLED=false` 时，应用不会自行执行该迁移。
升级前先备份 PostgreSQL，在维护步骤中执行并登记迁移（执行方式见上文
「执行数据库迁移」），再同时替换所有连接同一数据库的应用节点；旧约束未删除前，
写入新平台值会被数据库拒绝。

本次合并后在测试机隔离目录完成了源码级验证：`go build ./...`、`go test -tags=unit`
（58 个包）与 `go test -tags=integration`（需要 testcontainers 的
`postgres:18.1-alpine3.23`、`redis:8.4-alpine` 镜像）全部通过；前端
`pnpm install --frozen-lockfile`、`vue-tsc`、eslint 与关键 vitest（31 个文件、
579 个用例）全部通过。

`0.4.15` 基于本项目 `0.4.14`，合并上游 `v0.2.16`（81 个提交，合并基点 `f2669c8cf`，
即上游 `v0.2.15`）。上游主要变化：Gemini 显式上下文缓存（AI Studio / Vertex / 自建
网关）与配套计费、Claude Haiku 5.5 接入、GPT-6.1 Sol 上下文窗口扩到 500k、分组名称
国际化（group i18n）、国际编码端点识别，以及风控、监控、Gemini 工具结果图片、
WebSocket 并发与额度配额等多项修复。

合并只在四个文件产生冲突，均按本项目处理：`README.md`、`README_CN.md`、
`README_JA.md` 保留本项目版本，`backend/cmd/server/VERSION` 写为本项目 `0.4.15`。
其余两侧都改过的 12 个文件由 Git 自动合并，合并后逐项复核本项目自定义逻辑仍在位：

- 账号级限制：`GetAccountPlanType`、`SupportsCodexClientRestriction`，OpenAI OAuth /
  SetupToken / API Key 三种形态共用「仅允许 Codex 官方客户端」与「仅允许 Responses
  入口」两个开关（`backend/internal/service/account.go`）。
- 入口与客户端 failover：`openAIInboundProtocolFailover`、
  `openAICodexRestrictionFailoverEnabled`、`ensurePluginClientConversationContext`
  （`openai_gateway_forward.go`、`openai_gateway_chat_completions.go`）。
- Codex 模型缓存：`openAIModelsCacheFresh` 在单飞闭包内二次确认新鲜度。
- 风控：`bindContentModerationAccountScope`、`securityAuditAccountPlanPending`，以及
  `account_types` / `account_ids` / `account_plan_types` 并集范围与 Gemini、Anthropic
  Messages 入口的账号级审计（`content_moderation.go`、`gateway_handler.go`）。
- 默认安全开关：`database.initialization_enabled=false`、
  `subscription_maintenance.auto_reset_enabled=false`（`config.go`、
  `deploy/config.example.yaml`）。
- 前端账号表单：`responsesOnly` 开关、`codexCLIOnly` 支持 API Key 类型、中英文文案。

上游本次新增四个幂等迁移：

- `242_group_i18n.sql`：`groups` 表新增 `i18n` JSONB 列（默认 `{}`）。
- `243_gemini_cached_contents.sql`：新建 `gemini_cached_contents` 表及相关索引。
- `244_gemini_cached_contents_channel_usage.sql`：新增 `channel_usage` JSONB 列。
- `245_gemini_cached_contents_mutation_lease.sql`：新增 `mutation_token`、
  `mutation_lease_until` 列与待处理变更索引。

生产环境保持 `DATABASE_INITIALIZATION_ENABLED=false` 时，应用不会自行执行这四个迁移。
升级前先备份 PostgreSQL 与 compose/env，在维护窗口执行并登记迁移（方式见上文
「执行数据库迁移」），再同时替换所有连接同一数据库的应用节点；`groups.i18n` 列与
`gemini_cached_contents` 表建立前，新版本应用写入会报错。

`contrib/codex-native-transport/`（Codex Native Transport 插件）是本项目新增目录，
上游没有该路径，合并不会改动插件源码。

合并后在本机（Windows，Go 1.27.2 工具链 + 现有 `node_modules`）完成验证：

- 后端 `go build ./...` 通过。
- 后端 `go test -tags=unit ./...`：除 `TestOllamaProbeCallback_StaleLongDoesNotOverrideNewShort`
  外全部通过；该用例在合并前的 `0.4.14` 上同样失败，属既有问题，与本次合并无关。
  `TestCommandCodeGatewayPassesThroughCatalogProtocols` 全量运行偶发失败、单独运行
  通过，两侧均未改动该文件，属既有偶发用例。
- 前端 `vue-tsc --noEmit`、`vue-tsc -b`、`vitest run`（372 个文件 / 2870 个用例）与
  `vite build` 全部通过。

### 0.4.15 升级执行记录（2026-10-15）

测试机 `154.217.250.100`：备份 `/www/wwwroot/quya-sub2api-dev-runtime/backups/20261014-071145/`
（compose、`.env`、`sub2api_dev-pre-0.4.15.dump`），手工执行并登记 242–245 后切镜像，
容器 healthy，健康检查、管理员登录、公开配置接口均正常。

正式机 `142.54.187.42` 与 `107.150.62.202` 使用同一个数据库（142 本机 PostgreSQL 18，
库 `sub2api`，两台应用都指向它），因此迁移只执行一次。

- 升级前全量备份：`pg_dump -Fd -j 8 -Z 6` 输出到
  `/www/wwwroot/sb/backups/0.4.15-20261014T155755Z/sub2api.d`（9.8 GB，rc=0）；两台应用
  目录的 compose、`.env`、镜像 ID、容器清单也备份在同前缀目录下。
- 242–245 在维护步骤中手工执行并登记 `schema_migrations`（校验和与仓库 blob 一致）：
  `groups.i18n`（jsonb NOT NULL DEFAULT `{}`）与 `gemini_cached_contents`（含 5 个索引）
  已建立，行数校验 users 12732 / accounts 48429 / groups 71 / user_subscriptions 4098 不变。
- 两台在同一 UTC 时刻（`1791994760`）同时执行 `docker compose up -d --no-deps sub2api`，
  容器均 healthy、restarts=0，`/health` 返回 200，插件面板 8848 正常；升级后 5 分钟内
  142 侧 287 个请求全部 2xx/3xx/4xx，无 5xx。107 侧实例本身几乎不接流量（30 分钟 3 个
  请求，均为验证探针），属既有现象。
- 回滚方式：把 compose 镜像标签改回 `0.4.14` 后 `docker compose up -d --no-deps sub2api`
  重建 `sub2api-sb`。本次迁移只新增列和表，与 0.4.14 向后兼容，代码回滚无需恢复数据库。

### 号池 vm2api 升级记录（2026-10-15）

生产 `107.150.62.202` 的 `vm2api`（`/opt/vm2api`，host 网络 8787）从 v1.3.159 升到上游最新
`v1.3.169`。

- 补丁现状：#371 / #384 / #386 / #389 早已进上游，**#412（批量 pool-policy + 可 PATCH 权重）
  从 v1.3.168 起也在上游**；只剩 `POST /api/panel/vms/:id/credential/clear` 需要本地补丁。
  已按 v1.3.169 上游 `panel-routes.mjs` 重打（+84 行），放 `/opt/vm2api/patches/v1.3.169/`，
  compose 只读挂载覆盖。
- 备份：`/opt/vm2api/backups/pre-v1.3.169-<stamp>/`（compose、`.env`、`src/config`、`patches`），
  数据库一致快照 `/root/vm2api-db-snapshots/kin-pre169-20261014T165749Z.db`（2.9 GB，`VACUUM INTO`）。
- `docker compose up -d --no-deps vm2api` 只重建控制面容器。`src/config/distill-rules.json`
  需要手工换成新版默认：旧文件是 159 版默认 + 一句 `<think>` 针，159 代码加载时会丢弃这句，
  169 会保留，直接沿用会让 NewAPI 类转发误 403。`bin/` 与 `share/wrap-cli` 由镜像入口自动刷新，
  Codex 槽的 `kin-codex-kernel` 随重启生效（`wrap-cli/sync` 只处理非 Codex 槽，本池只有 vm-01）。
- 新迁移 `037_usage_logs_billing_normalize.sql` 在启动时自动执行（37.9 万行，实测 4.5 s）：
  `input_tokens` 改存不含缓存的量，面板总量由新代码把 cache_read / cache_creation 加回来。
- 回滚：`.env` 的 `VM2API_IMAGE_TAG` 改回 `v1.3.159` 再 `docker compose up -d --no-deps vm2api`；
  037 已执行，回 159 会让历史 OpenAI 行的提示 token 统计偏低，要连 DB 快照一起恢复才准确。
- 副作用：重启期间 8787 有几秒 refused，sub2api 把 `vm2api号池` / `VM2号池 LUNA` 临时停调度
  10 分钟（`openai.account_temp_unscheduled_transport ... until=<+10min>`），之后自动恢复。

### 号池 vm2api 补丁全部并入上游 · 已备 v1.3.174（2026-10-15）

- 上游当天连发 v1.3.170–v1.3.174。发起的最后一个本地补丁随 **PR #438 于 2026-10-15 00:17 UTC 合并**
  （合并提交 `ccb3f2a`，上游同时收下了本次提交的 e2e 用例），v1.3.170 起
  `POST /api/panel/vms/:id/credential/clear` 已是上游能力 → **本地补丁清零**，历史上挂过的
  pool-policy 批量/权重、account scope、槽位记录读缓存、GPT 槽 no_credential 判定等补丁更早已在上游。
- 107 侧升级**已完成**（用户放行后重启）：`docker compose up -d --no-deps vm2api`，
  容器 `image=ghcr.io/dofastted/vm2api:v1.3.174`、`restarts=0`；`.env` 的 `VM2API_IMAGE_TAG=v1.3.174`，
  compose 已撤掉 `patches/v1.3.169/panel-routes.mjs` 只读挂载（撤挂载后 `credential/clear` 仍 404 `vm_not_found`
  校验通过，证明路由已在镜像内）。容器启动时间 2026-10-15 16:51:42 UTC。
- 升级后核验：`/api/panel/version` = 1.3.174；池内 101 个槽容器在运行、无重建；直连
  `POST /v1/responses`（`gpt-6-astra`）返回 200；models.dev 价格同步首次 `status:error`，
  用 `POST /api/panel/pricing/sync` 重跑后 `status:ok`（70 个模型），与上次升级同现象。
- sub2api 侧副作用如预期：重启瞬间 8787 refused，`vm2api号池` / `VM2号池 LUNA` 被
  `openai.account_temp_unscheduled_transport` 临时停调度（until 17:01 UTC），`16:56` 已自动清空；
  恢复后 15 分钟内两账号分别接到 755 / 29 条请求，共 3804 条系统请求。20 分钟窗口内
  `107.150.62.202:8787` 的传输错误 23 次，分布在重启窗口与重启前（16:36 / 16:40 各有一批，
  属既有的代理偶发超时），重启后最近 5 分钟无新增。
- 备份：`/opt/vm2api/backups/pre-v1.3.174-20261015T112515Z/`（compose、`.env`、`vms-json/` 103 个槽记录）。
- 配置核对：v1.3.174 镜像 `src/config` 默认值与现网逐文件比对，`distill-rules.json`
  （169 时手工换过的那份）、`intercept-rules.json`、`gateway-v2.example.json` 哈希一致，无需再动；
  `routing.json` 是运行态文件，按上游要求不覆盖（缺 `notify.wecom` 块，企业微信通道默认关，面板保存一次即可）。
- v1.3.170–v1.3.174 无数据库迁移、无新依赖；v1.3.170 首次启动会把挂在 `px-local` 上的节点槽
  迁到 `px-local-<节点>` 并改写 `vms/<id>.json` 的 `proxy.id`。
- 生效命令（待用户放行后再执行）：`cd /opt/vm2api && docker compose up -d --no-deps vm2api`。
  副作用同前：8787 短暂 refused，sub2api 会把 `vm2api号池` / `VM2号池 LUNA` 临时停调度 10 分钟。
- 仓库：`jxb412/vm2api` 的 `main` 已快进到上游 `38c7fb9`（v1.3.174）并推送；本地工作树
  `D:\btc\st\.tmp\vm2api-pr` 的 `main` 同点，PR 分支 `feat/slot-credential-clear` 保留备查。

### 号池 vm2api 升到 v1.3.175（2026-10-16）

- v1.3.175 变更（无数据库迁移、无新依赖，不必 `wrap-cli/sync`，不必重建槽位容器）：
  - 新增槽位「恢复服务」：`GET /api/panel/vms/:id/diagnose` 只读诊断（槽位状态、凭证、节点 SSH 与集群中继、
    出口代理与本机 kin-egress、容器、推理内核、自动恢复、调度），`POST /api/panel/vms/:id/recover`
    执行关机→开机；同槽并发返回 409 `recover_in_progress`；租户可对自己的槽调用（#457）。
  - 模型测试失败时突出错误码与 HTTP 状态，5xx 自动诊断并给出恢复入口。
  - 看门狗补缺：容器缺失或已退出时整槽开机；每轮检查本机 kin-egress 与集群 docker 桥 / 槽位中继并拉起；
    控制面启动后对「记录为运行中但容器不存在或未运行」的槽补开机一次（`kernel_watchdog.enabled=false` 时不做）。
- 107 生产机升级：只重建 `vm2api` 控制面（`cd /opt/vm2api && docker compose up -d --no-deps vm2api`），
  槽位容器全部保留、未重建；`.env` → `VM2API_IMAGE_TAG=v1.3.175`，镜像 digest
  `sha256:47aea38e5cbe918a0bb9e0172c4c31561455553023501b40db43cc8a97693806`。
- 备份：`/opt/vm2api/backups/pre-v1.3.175-20261015T170841Z/`（compose、`.env`、`vms-json/` 103 个槽记录）。
- 配置核对：v1.3.174→v1.3.175 的 `src/config` 四个文件（`distill-rules.json`、`intercept-rules.json`、
  `gateway-v2.example.json`、`routing.json`）零差异，镜像内哈希与现网一致，未手改配置；
  `routing.json`、`vms/`、`data/`、`.env` 全部保留。
- 重启时间 2026-10-15 17:09:12 UTC（北京 10-16 01:09），容器 `restarts=0`。
- 核验：`/api/panel/version` = 1.3.175、`update_available=false`、`needs_wrap_cli_sync=false`；
  池内 101 槽 `running` + 1 `stopped`（与升级前一致）；直连 `POST /v1/responses`（`gpt-6-astra`）返回 200；
  models.dev 价格同步首次即 `status:ok`（70 个模型，本轮无需手动补跑 `/api/panel/pricing/sync`）；
  看门狗无异常补开机记录。
- sub2api 侧副作用（与 174 同）：重启瞬间 8787 连接被拒，`vm2api号池`(47942) 被打上
  `openai.account_temp_unscheduled_transport`，`temp_unschedulable_until = 17:19:11 UTC`（+10 分钟）；
  实际 17:14:30 就被一次成功请求清空并恢复调度（中断 5 分 18 秒）。`VM2号池 LUNA`(48044) 只断了 35 秒，
  17:09:47 即恢复。17:22 复查两侧 `temp_unschedulable_until` 均为空、`last_used` 实时更新，号池调度正常。
- 回滚：`.env` 改回 `VM2API_IMAGE_TAG=v1.3.174`，重复同一条 `docker compose up -d --no-deps vm2api`。
- 仓库：本地工作树 `D:\btc\st\.tmp\vm2api-pr` 的 `main` 已快进到上游 `60747c7`（含 v1.3.175 提交 `743652a`）。

## 上游同步

```powershell
git status --short
git fetch origin
git fetch upstream
git switch -c sync/upstream-YYYY-MM-DD origin/main
git merge --no-ff upstream/main
```

冲突时保留个人自定义逻辑并逐项测试。重点检查启动、更新源、OpenAI 调度、风控、
部署 Compose 和迁移文件。不要使用 `git reset --hard` 或 `git checkout --` 清理
工作区。

## 验证命令

```bash
cd backend
go test -tags=unit ./...
go test -tags=integration ./...
golangci-lint run ./...

cd ../frontend
pnpm install --frozen-lockfile
pnpm typecheck
pnpm test:run
pnpm build
```

本地缺少 Go、pnpm、PostgreSQL 或 Redis 时，应明确记录检查未执行。

## 线上操作底线

- 不要未经确认重启服务器或生产服务。
- 不要删除或更换 PostgreSQL、Redis、`/app/data` 数据卷。
- 不要把生产密钥、OAuth token、API 秘钥、代理密码或完整 attestation 写入 Git 和日志。
- 应用、数据库、Redis、配置和镜像更新前都要保留可恢复备份。
- 已应用 migration 不可修改；新增结构必须创建新的递增 migration。

## 相关文档

- [中文项目入口](../README_CN.md)
- [开发指南](../DEV_GUIDE.md)
- [部署说明](../deploy/README.md)
- [Docker 说明](../deploy/DOCKER.md)
- [边缘安全](../deploy/EDGE_SECURITY.md)
- [配置模板](../deploy/config.example.yaml)
- [支付说明](PAYMENT_CN.md)
- [组合分组](COMPOSITE_GROUPS.md)
- [异步图片任务](ASYNC_IMAGE_TASKS.md)
