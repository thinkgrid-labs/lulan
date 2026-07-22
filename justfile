# Lulan dev commands. Install `just`: https://github.com/casey/just

# List available commands
default:
    @just --list

# Start the dev infrastructure (Postgres + Redis)
up:
    docker compose -f deploy/compose/dev.yml up -d

# Stop the dev infrastructure
down:
    docker compose -f deploy/compose/dev.yml down

# Run the API server against the dev stack
serve:
    DATABASE_URL=postgres://lulan:lulan@localhost:5432/lulan \
    REDIS_URL=redis://localhost:6379 \
    LULAN_QUOTE_SECRET=dev-only-quote-secret-not-for-production \
    cargo run -p lulan-api

# Format, lint, test — what CI runs
check:
    cargo fmt --all -- --check
    cargo clippy --workspace --all-targets -- -D warnings
    cargo test --workspace

# Auto-fix formatting
fmt:
    cargo fmt --all

# Build the production Docker image
docker-build:
    docker build -t lulan-api:dev .

# Run the 10k-contender double-sell invariant harness against a running server
loadgen contenders="10000" hold_ratio="0.0":
    DATABASE_URL=postgres://lulan:lulan@localhost:5432/lulan \
    CONTENDERS={{contenders}} HOLD_RATIO={{hold_ratio}} \
    cargo run --release -p lulan-loadgen

# Paced open-loop latency run (honest seat-lock numbers vs the <20ms target)
loadgen-paced rate="200" duration="30" hold_ratio="0.0":
    DATABASE_URL=postgres://lulan:lulan@localhost:5432/lulan \
    MODE=paced RATE={{rate}} DURATION_SECS={{duration}} HOLD_RATIO={{hold_ratio}} \
    cargo run --release -p lulan-loadgen

# Release-build server for benchmarking (rate limiter opened up)
serve-release:
    DATABASE_URL=postgres://lulan:lulan@localhost:5432/lulan \
    REDIS_URL=redis://localhost:6379 \
    LULAN_QUOTE_SECRET=dev-only-quote-secret-not-for-production \
    LULAN_RATE_LIMIT=10000000 LULAN_DB_POOL=50 \
    cargo run --release -p lulan-api

# Production stack, external databases (build image first: just docker-build)
prod-up:
    docker compose -f deploy/compose/production.yml up -d

# Production stack, fully self-contained (bundled Postgres + Redis)
prod-up-bundled:
    docker compose -f deploy/compose/production.yml -f deploy/compose/bundled-db.yml up -d

prod-down:
    docker compose -f deploy/compose/production.yml -f deploy/compose/bundled-db.yml down
