# Quya Sub2API

Quya Sub2API is the personal maintained distribution of the Sub2API gateway.
It provides account management, API-key authentication, quota billing, model
routing, failover, concurrency control, and an administration console for
OpenAI, Anthropic, Gemini, Grok, and compatible providers.

- Repository: <https://github.com/jxb412/quya-sub2api>
- Upstream source: <https://github.com/Wei-Shaw/sub2api>
- Chinese documentation: [README_CN.md](README_CN.md)

## Scope

This repository contains the gateway backend, Vue frontend, deployment files,
database migrations, and project documentation. It does not include unrelated
proxy, payment-frontend, inspection, or sibling project directories.

The custom distribution includes OpenAI inbound protocol pre-selection,
independent `openai_responses_only` routing, Codex restriction-aware failover,
risk-control filtering by account plan type, release checks against this
repository, and opt-in first-run/database initialization.

## Important Notes

- Do not run `git reset --hard`, `git checkout --`, or destructive data-volume commands against this repository or production deployment.
- Do not reboot or restart production services without an explicit maintenance decision.
- Back up PostgreSQL, Redis, `.env`, configuration files, and Docker data volumes before upgrades.
- Applied migrations are immutable. Add a new migration file instead of editing an applied migration.
- Production defaults are `SETUP_ENABLED=false`, `AUTO_SETUP=false`, and `DATABASE_INITIALIZATION_ENABLED=false`.
- These flags disable first-run installation and startup schema/bootstrap writes; normal application database reads and writes remain enabled.
- If an upstream release adds migrations, temporarily enable `DATABASE_INITIALIZATION_ENABLED=true`, validate the upgrade, then disable it again.

## Deployment

The recommended production deployment is Docker Compose with local data directories:

```bash
cp deploy/.env.example deploy/.env
# Set POSTGRES_PASSWORD, JWT_SECRET, TOTP_ENCRYPTION_KEY, and deployment values.
cd deploy
docker compose -f docker-compose.local.yml up -d
docker compose -f docker-compose.local.yml ps
docker compose -f docker-compose.local.yml logs --tail=100 sub2api
```

For a brand-new installation, explicitly set all three variables in `.env`:

```dotenv
SETUP_ENABLED=true
AUTO_SETUP=true
DATABASE_INITIALIZATION_ENABLED=true
```

After installation, set them back to `false`. Preserve the existing `/app/data`,
PostgreSQL, and Redis volumes in production.

Read [deploy/README.md](deploy/README.md), [deploy/EDGE_SECURITY.md](deploy/EDGE_SECURITY.md),
and [deploy/config.example.yaml](deploy/config.example.yaml) for full deployment,
backup, reverse-proxy, and configuration instructions.

## Release and Update

The GitHub Actions release workflow is triggered by a stable `v*` tag. After CI
passes, create a tag such as `v0.1.180`. The workflow publishes:

```text
ghcr.io/jxb412/sub2api:0.1.180
ghcr.io/jxb412/sub2api:latest
```

Docker servers pull the published image and recreate only the application
container; they do not need to build the image locally. The Compose image must
point to the custom repository. `docker pull` alone does not restart services.

The built-in release checker reads releases from this repository and is intended
for binary/systemd deployments. Docker deployments still pull and recreate the
application image.

To merge upstream changes without replacing custom work:

```bash
git fetch origin
git fetch upstream
git switch -c sync/upstream-YYYY-MM-DD origin/main
git merge --no-ff upstream/main
```

Resolve conflicts explicitly, run the checks, and merge the sync branch only
after review. Never replace `main` with `upstream/main` using a hard reset.

## Project Layout

```text
backend/       Go gateway, handlers, services, repositories, migrations
frontend/      Vue 3 frontend, API clients, views, stores, i18n
deploy/        Docker Compose, Dockerfiles, environment/config templates
docs/          Feature, payment, API, operations, and security documentation
openspec/      Project proposals and design records
```

## Development Checks

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

Use Go 1.26.6 and pnpm. If a tool is unavailable, report that check as not run.

## Documentation Index

- [Chinese project guide](README_CN.md)
- [Project handoff and operations](docs/PROJECT_HANDOFF_CN.md)
- [Development guide](DEV_GUIDE.md)
- [Deployment guide](deploy/README.md)
- [Docker image notes](deploy/DOCKER.md)
- [Edge security](deploy/EDGE_SECURITY.md)
- [Payment](docs/PAYMENT.md)
- [Composite groups](docs/COMPOSITE_GROUPS.md)
- [Async image tasks](docs/ASYNC_IMAGE_TASKS.md)
- [Admin payment integration API](docs/ADMIN_PAYMENT_INTEGRATION_API.md)

Review [LICENSE](LICENSE) and [docs/legal/](docs/legal/). Operate the service only with
the required upstream authorization and in accordance with applicable laws,
provider terms, privacy obligations, and customer agreements.
