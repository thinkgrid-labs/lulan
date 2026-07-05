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
