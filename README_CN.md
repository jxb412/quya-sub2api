# Quya Sub2API

Quya Sub2API 是 Sub2API 的个人维护版本，提供 AI API 网关、账号管理、API
秘钥鉴权、额度计费、模型路由、故障转移、并发控制和管理后台，支持 OpenAI、
Anthropic、Gemini、Grok 及兼容上游。

- 个人仓库：<https://github.com/jxb412/quya-sub2api>
- 上游仓库：<https://github.com/Wei-Shaw/sub2api>
- English: [README.md](README.md)

## 项目范围

本仓库只包含网关后端、Vue 前端、部署文件、数据库迁移和项目文档。不包含
IPv6 代理、支付前端、版本检查副本或其他兄弟项目目录。

当前个人版本的主要改动：

- OpenAI 入口协议预筛选：Responses、Chat Completions、Messages。
- 独立的 `openai_responses_only` 账号限制。
- Codex 客户端限制与支持 failover 的账号调度。
- 风控中心按 `pro`、`plus`、`team`、`free` 账号类型筛选。
- 内置更新检查指向 `jxb412/quya-sub2api`。
- 首次安装和启动阶段数据库初始化默认关闭。

## 重要注意事项

- 不要对项目或生产环境执行 `git reset --hard`、`git checkout --` 或删除数据卷。
- 不要未经确认重启生产服务器或业务容器。
- 升级前备份 PostgreSQL、Redis、`.env`、配置文件和 Docker 数据目录。
- 已执行的数据库迁移不可修改；新增结构必须创建新的递增迁移文件。
- 默认关闭：`SETUP_ENABLED=false`、`AUTO_SETUP=false`、`DATABASE_INITIALIZATION_ENABLED=false`。
- 默认关闭只代表不执行首次安装和启动时的迁移/初始化写入；业务请求仍会正常读写数据库。
- 如果上游版本包含新的数据库迁移，升级时临时设置 `DATABASE_INITIALIZATION_ENABLED=true`，验证完成后恢复为 `false`。

## 部署方式

生产环境推荐使用带本地数据目录的 Docker Compose：

```bash
cp deploy/.env.example deploy/.env
# 设置 POSTGRES_PASSWORD、JWT_SECRET、TOTP_ENCRYPTION_KEY 等配置
cd deploy
docker compose -f docker-compose.local.yml up -d
docker compose -f docker-compose.local.yml ps
docker compose -f docker-compose.local.yml logs --tail=100 sub2api
```

只有真正进行新安装时，才打开以下三个变量：

```dotenv
SETUP_ENABLED=true
AUTO_SETUP=true
DATABASE_INITIALIZATION_ENABLED=true
```

安装完成后改回 `false`。已有生产部署必须保留原来的 `/app/data`、PostgreSQL
数据卷和 Redis 数据卷，不要换项目目录或 Compose 项目名导致创建新卷。

完整说明请阅读：

- [部署说明](deploy/README.md)
- [Docker 说明](deploy/DOCKER.md)
- [边缘代理安全](deploy/EDGE_SECURITY.md)
- [配置模板](deploy/config.example.yaml)

## 更新与发布

GitHub Actions 在稳定的 `v*` 标签上构建发布包和镜像。CI 通过后，例如创建
`v0.1.190`，会发布：

```text
ghcr.io/jxb412/sub2api:0.1.190
ghcr.io/jxb412/sub2api:latest
```

Docker 服务器直接拉取自己的镜像并只重建应用容器，不需要在服务器上重新编译。
当前 Compose 的 `image` 必须改成 `ghcr.io/jxb412/sub2api:<版本>`。单独执行
`docker pull` 不会重启服务。

内置“检查更新”已经查询个人仓库，主要用于二进制/systemd 部署。Docker 部署仍
然需要拉取新镜像并重建应用容器。

同步上游但保留自定义改动：

```bash
git fetch origin
git fetch upstream
git switch -c sync/upstream-YYYY-MM-DD origin/main
git merge --no-ff upstream/main
```

冲突必须逐项审查和测试。不要用 `upstream/main` 强行覆盖个人 `main`。

## 项目结构

```text
backend/       Go 网关、处理器、服务、数据访问、迁移
frontend/      Vue 3 前端、API、页面、状态和国际化
deploy/        Docker Compose、Dockerfile、环境和配置模板
docs/          功能、支付、API 和运维文档
openspec/      项目设计提案和变更记录
```

## 开发检查

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

后端使用 Go 1.26.6，前端使用 pnpm。工具未安装时必须明确记录为未执行。

## 文档索引

- [项目交接与运维](docs/PROJECT_HANDOFF_CN.md)
- [开发指南](DEV_GUIDE.md)
- [部署说明](deploy/README.md)
- [支付功能](docs/PAYMENT_CN.md)
- [组合分组](docs/COMPOSITE_GROUPS.md)
- [异步图片任务](docs/ASYNC_IMAGE_TASKS.md)
- [后台支付接口](docs/ADMIN_PAYMENT_INTEGRATION_API.md)

请阅读 [LICENSE](LICENSE) 和 [docs/legal/](docs/legal/) 中的合规说明，并遵守上游服务条款、
隐私义务和适用法律。
