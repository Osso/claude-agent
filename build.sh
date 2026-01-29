#!/usr/bin/env bash
set -euo pipefail

REGISTRY="registry.digitalocean.com/globalcomix"
TAG="${1:-latest}"

echo "=== Building claude-agent (tag: $TAG) ==="

# Build server
echo ""
echo "=== Building server ==="
docker build -q -f Dockerfile.server \
    -t "$REGISTRY/claude-agent-server:$TAG" \
    -t "$REGISTRY/claude-agent-server:latest" \
    .

# Build worker
echo ""
echo "=== Building worker ==="
GITLAB_CLI_DIR="${GITLAB_CLI_DIR:-$HOME/Projects/cli/gitlab}"
GITHUB_CLI_DIR="${GITHUB_CLI_DIR:-$HOME/Projects/cli/github}"
SENTRY_CLI_DIR="${SENTRY_CLI_DIR:-$HOME/Projects/cli/sentry}"
docker build -q -f Dockerfile.worker \
    --build-context "gitlab-cli=$GITLAB_CLI_DIR" \
    --build-context "github-cli=$GITHUB_CLI_DIR" \
    --build-context "sentry-cli=$SENTRY_CLI_DIR" \
    -t "$REGISTRY/claude-agent-worker:$TAG" \
    -t "$REGISTRY/claude-agent-worker:latest" \
    .

echo ""
echo "=== Build complete ==="
echo "Images built:"
echo "  $REGISTRY/claude-agent-server:$TAG"
echo "  $REGISTRY/claude-agent-worker:$TAG"
