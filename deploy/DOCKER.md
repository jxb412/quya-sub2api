# Quya Sub2API Docker 镜像

个人仓库的稳定版本通过 `v*` 标签发布到 GitHub Container Registry：

```text
ghcr.io/jxb412/sub2api:<version>
ghcr.io/jxb412/sub2api:latest
```

生产环境建议固定版本：

```bash
docker pull ghcr.io/jxb412/sub2api:0.1.190
```

Compose 示例：

```yaml
services:
  sub2api:
    image: ghcr.io/jxb412/sub2api:0.1.190
    restart: unless-stopped
    ports:
      - "8080:8080"
    environment:
      SETUP_ENABLED: "false"
      AUTO_SETUP: "false"
      DATABASE_INITIALIZATION_ENABLED: "false"
    volumes:
      - ./data:/app/data
```

完整的 PostgreSQL、Redis、环境变量、首次安装、升级、迁移、备份和回退说明见
[README.md](README.md)。

`docker pull` 只下载镜像，不启动容器、不初始化数据库，也不中断现有业务。真正使用
新镜像需要重新创建应用容器。不要执行 `docker compose down -v`。
