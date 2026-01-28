#!/usr/bin/env bash
set -euo pipefail

REGISTRY="registry.digitalocean.com/globalcomix"
TAG="${1:-latest}"
NAMESPACE="claude-agent"

echo "=== Deploying claude-agent (tag: $TAG) ==="

# Build and push server
echo ""
echo "=== Building server ==="
docker build -f Dockerfile.server \
    -t "$REGISTRY/claude-agent-server:$TAG" \
    -t "$REGISTRY/claude-agent-server:latest" \
    .

echo ""
echo "=== Pushing server ==="
docker push "$REGISTRY/claude-agent-server:$TAG"
docker push "$REGISTRY/claude-agent-server:latest"

# Build and push worker
echo ""
echo "=== Building worker ==="
GITLAB_CLI_DIR="${GITLAB_CLI_DIR:-$HOME/Projects/cli/gitlab}"
docker build -f Dockerfile.worker \
    --build-context "gitlab-cli=$GITLAB_CLI_DIR" \
    -t "$REGISTRY/claude-agent-worker:$TAG" \
    -t "$REGISTRY/claude-agent-worker:latest" \
    .

echo ""
echo "=== Pushing worker ==="
docker push "$REGISTRY/claude-agent-worker:$TAG"
docker push "$REGISTRY/claude-agent-worker:latest"

# Restart deployment
echo ""
echo "=== Restarting server deployment ==="
kubectl rollout restart deployment/claude-agent-server -n "$NAMESPACE"

echo ""
echo "=== Waiting for rollout ==="
kubectl rollout status deployment/claude-agent-server -n "$NAMESPACE" --timeout=60s

echo ""
echo "=== Deployment complete ==="
kubectl get pods -n "$NAMESPACE"
