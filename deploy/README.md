# Deploying Lulan

Two supported tiers:

| Tier | For | Stack |
|---|---|---|
| **Compose** (this page) | Most operators | API + Caddy (auto-TLS); **databases external by default** (RDS, Cloud SQL, ElastiCache, your own) — or add the bundled overlay for a fully self-contained machine |
| **Kubernetes** | Larger fleets | Helm chart in [`k8s/lulan`](k8s/lulan) — same external-database model |

## Compose install (target: first booking in < 30 minutes)

Prerequisites: a Linux host with Docker + the compose plugin, a DNS record
pointing your domain at it, ports 80/443 open.

```bash
# 1. Get the code and build the image (~5 min)
git clone https://github.com/thinkgrid-labs/lulan.git && cd lulan
docker build -t lulan-api:latest .

# 2. Configure: domain, secrets, and your database URLs
cp deploy/compose/.env.example deploy/compose/.env
$EDITOR deploy/compose/.env        # secrets: openssl rand -hex 32

# 3a. Launch against your EXTERNAL Postgres/Redis (default)
docker compose -f deploy/compose/production.yml up -d

# 3b. …or fully self-contained (bundled Postgres + Redis on this machine)
docker compose -f deploy/compose/production.yml \
               -f deploy/compose/bundled-db.yml up -d

# 4. Verify — Caddy fetches the TLS certificate automatically
curl https://$YOUR_DOMAIN/health/ready
```

The API needs only two connection strings — `DATABASE_URL` (required) and
`REDIS_URL` (optional) — so any Postgres 14+ and Redis 7 work: managed
cloud services, an existing cluster, or the bundled containers.

Migrations run automatically on API start. To load the demo network for a
test drive (skip for a real operation — use the GTFS importer instead):

```bash
docker compose -f deploy/compose/production.yml exec api lulan-api seed
```

First booking: follow the Quick start in the repo README against your
domain. Your admin credential is the `LULAN_BOOTSTRAP_ADMIN_KEY` you set —
use it to mint scoped keys (`POST /v1/api-keys`), register webhooks, and
manage the ancillary catalog; then rotate the bootstrap key away.

## Importing your schedule (GTFS)

Lulan ingests the industry-standard GTFS feed most operators already have
(agency, stops, routes, trips, stop_times, calendar):

```bash
docker compose -f deploy/compose/production.yml \
  exec -v /path/to/gtfs:/gtfs api \
  lulan-api import-gtfs /gtfs --days 30 --seats 40
```

See `lulan-api import-gtfs --help` for capacity mapping — GTFS carries no
seat maps, so trips are attached to a vehicle template you describe (or an
existing resource via `--vessel CODE`).

## Backup & restore

Everything that matters lives in PostgreSQL (orders, events, tickets,
keys). Redis holds only expiring soft holds — safe to lose.

**External databases**: use your provider's backup story (RDS snapshots,
`pg_dump` against the URL, PITR). Lulan needs nothing special — one
logical database.

**Bundled overlay**:

```bash
# Nightly backup (add to cron)
docker compose -f deploy/compose/production.yml -f deploy/compose/bundled-db.yml \
  exec -T postgres pg_dump -U lulan -Fc lulan > lulan-$(date +%F).dump

# Restore onto a fresh stack
docker compose -f deploy/compose/production.yml -f deploy/compose/bundled-db.yml up -d postgres
docker compose -f deploy/compose/production.yml -f deploy/compose/bundled-db.yml \
  exec -T postgres pg_restore -U lulan -d lulan --clean --if-exists < lulan-YYYY-MM-DD.dump
docker compose -f deploy/compose/production.yml -f deploy/compose/bundled-db.yml up -d
```

Test your restore path before you need it. The Ed25519 ticket-signing keys
are rows in `ticket_keys` and travel with the dump — restored tickets keep
validating offline.

## Upgrades

```bash
git pull && docker build -t lulan-api:latest .
docker compose -f deploy/compose/production.yml up -d api
```

Migrations are forward-only and run on boot. Take a backup first; roll
back by restoring it and starting the previous image tag.

## Operational notes

- `/metrics` (Prometheus) is blocked at Caddy; scrape it from inside the
  network (`api:8080/metrics`).
- The API is stateless — scale it horizontally behind Caddy by adding
  replicas; the outbox relay and webhook worker coordinate through
  Postgres row locks (`FOR UPDATE SKIP LOCKED`), so multiple instances
  are safe.
- Losing Redis degrades soft holds and rate limiting only; bookings stay
  correct (ADR 0002). The API also boots without Redis.
- WASM pricing modules are runtime artifacts: drop the file in
  `deploy/compose/modules/`, set `LULAN_PRICING_WASM=/modules/<file>`,
  restart `api` — no image rebuild.
