#!/bin/bash
set -e

cd "$(dirname "$0")"

case "${1:-}" in
    --unit)
        cargo test --workspace
        ;;
    --docker)
        echo "Building Docker images..."
        docker build -f Dockerfile.server -t claude-agent-server:test .
        docker build -f Dockerfile.worker -t claude-agent-worker:test .
        ;;
    *)
        cargo test --workspace
        cargo clippy --workspace -- -D warnings
        ;;
esac
