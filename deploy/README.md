# Quya Sub2API 部署与更新

本文档只说明 `jxb412/quya-sub2api` 的部署、更新、迁移和恢复注意事项。

## 推荐方式

生产环境推荐：

- Docker Compose v2。
- 固定版本镜像 `ghcr.io/jxb412/sub2api:<version>`。
- 使用 `docker-compose.local.yml` 将数据保存在部署目录。
- 反向代理只开放应用端口，不对公网开放 PostgreSQL 和 Redis。

项目提供的 Compose 文件：

| 文件 | 用途 |
|---|---|
| `docker-compose.local.yml` | 应用、PostgreSQL、Redis，本地目录持久化，生产推荐 |
| `docker-compose.yml` | 应用、PostgreSQL、Redis，Docker named volumes |
| `docker-compose.standalone.yml` | 只运行应用，连接外部数据库和 Redis |
| `docker-compose.dev.yml` | 从当前源码构建的开发环境 |

## 默认保护

个人发行版默认关闭安装和数据库启动初始化：

```dotenv
SETUP_ENABLED=false
AUTO_SETUP=false
DATABASE_INITIALIZATION_ENABLED=false
```

默认状态下：

- 不进入首次安装向导。
- 不自动创建数据库、管理员或配置文件。
- 不执行启动迁移、JWT 密钥补写和简单模式默认数据写入。
- 正常业务仍可读写已有数据库。

如果数据目录为空或挂载错误，应用会拒绝首次安装并退出，不会自动创建新环境。

## 新安装

新安装前准备目录和环境文件：

```bash
git clone https://github.com/jxb412/quya-sub2api.git
cd quya-sub2api/deploy
cp .env.example .env
mkdir -p data postgres_data redis_data
chmod 600 .env
```

编辑 `.env`，至少设置：

```dotenv
POSTGRES_PASSWORD=<strong-password>
JWT_SECRET=<64-character-random-hex>
TOTP_ENCRYPTION_KEY=<64-character-random-hex>

SETUP_ENABLED=true
AUTO_SETUP=true
DATABASE_INITIALIZATION_ENABLED=true
```

生成随机值：

```bash
openssl rand -hex 32
```

启动：

```bash
docker compose -f docker-compose.local.yml up -d
docker compose -f docker-compose.local.yml ps
docker compose -f docker-compose.local.yml logs --tail=200 sub2api
curl -fsS http://127.0.0.1:8080/health
```

确认安装成功后，将 `.env` 中三个保护变量恢复为 `false`，再只重建应用容器：

```bash
docker compose -f docker-compose.local.yml up -d --no-deps sub2api
```

## 已有环境首次切换到个人镜像

拉取镜像不会启动容器、不会连接数据库，也不会中断业务：

```bash
docker pull ghcr.io/jxb412/sub2api:<version>
```

更新前确认现有挂载：

```bash
docker inspect sub2api --format '{{range .Mounts}}{{println .Source "->" .Destination}}{{end}}'
```

必须保留原来的 `/app/data`、PostgreSQL 和 Redis 数据位置。修改现有 Compose 的
应用镜像，或使用覆盖文件：

```yaml
services:
  sub2api:
    image: ghcr.io/jxb412/sub2api:<version>
```

确保 `.env` 中为：

```dotenv
SETUP_ENABLED=false
AUTO_SETUP=false
DATABASE_INITIALIZATION_ENABLED=false
```

只替换应用容器：

```bash
docker compose -f docker-compose.local.yml -f docker-compose.custom.yml up -d --no-deps sub2api
```

这不会重启 PostgreSQL、Redis 或服务器操作系统。应用容器切换期间通常会有几秒
中断，正在进行的 SSE/WebSocket 请求可能需要客户端重试。

## 普通版本更新

1. 阅读个人仓库 Release 说明和数据库迁移清单。
2. 备份数据库、Redis、配置和部署文件。
3. 拉取固定版本镜像。
4. 只重建应用容器。
5. 检查健康状态、日志和关键 API。

```bash
docker pull ghcr.io/jxb412/sub2api:<new-version>
docker compose -f docker-compose.local.yml up -d --no-deps sub2api
docker compose -f docker-compose.local.yml ps
docker compose -f docker-compose.local.yml logs --tail=200 sub2api
curl -fsS http://127.0.0.1:8080/health
```

建议生产环境使用固定版本，不直接依赖 `latest`。

## 包含数据库迁移的更新

个人版本默认不会执行启动迁移。如果新版本新增 `backend/migrations/*.sql`：

1. 先创建 PostgreSQL 可恢复备份。
2. 审查新增 migration，不修改已应用的 migration。
3. 维护窗口临时设置：

```dotenv
DATABASE_INITIALIZATION_ENABLED=true
```

4. 只重建应用容器并观察迁移日志。
5. 检查 `schema_migrations`、健康接口和关键业务。
6. 恢复 `DATABASE_INITIALIZATION_ENABLED=false` 并再次只重建应用容器。

数据库迁移是正向操作。仅回退应用镜像不能自动回退表结构，必要时使用升级前备份。

## 备份

PostgreSQL 逻辑备份示例：

```bash
docker compose -f docker-compose.local.yml exec -T postgres \
  pg_dump -U "${POSTGRES_USER:-sub2api}" -d "${POSTGRES_DB:-sub2api}" \
  --format=custom > sub2api-before-upgrade.dump
```

同时备份：

- `.env` 和实际 `config.yaml`。
- `data/`。
- `postgres_data/` 或对应 named volume。
- `redis_data/` 或对应 named volume。

不要把包含密钥的备份提交到 Git。

## 回退

如果只修改了应用代码且没有执行新迁移，将 Compose 镜像改回旧固定版本并重建应用
容器即可。若执行过数据库迁移，先确认旧版本是否兼容新表结构；不兼容时恢复数据库
备份。

## 二进制/systemd 部署

从个人仓库 Releases 下载对应平台归档，将二进制部署到 `/opt/sub2api/`，配置文件
保存在 `/etc/sub2api/config.yaml`。升级前保留旧二进制和数据库备份。

内置“检查更新”读取 `jxb412/quya-sub2api` Releases。二进制替换完成后需要重启
`sub2api` systemd 服务，Docker 部署不要使用该方式替换容器内二进制。

## 反向代理

长连接接口需要正确代理 SSE 和 WebSocket。可信代理、客户端 IP 头、超时和缓存设置
请严格按照 [EDGE_SECURITY.md](EDGE_SECURITY.md) 配置。不要直接信任公网客户端传入的
转发头。

## 禁止操作

不要在生产环境执行：

```bash
docker compose down -v
```

也不要删除数据目录、随意更换 Compose 项目名、清空 volume、覆盖 `.env`，或在未备份
数据库时开启迁移。
