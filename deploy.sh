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
GITHUB_CLI_DIR="${GITHUB_CLI_DIR:-$HOME/Projects/cli/github}"
SENTRY_CLI_DIR="${SENTRY_CLI_DIR:-$HOME/Projects/cli/sentry}"
docker build -f Dockerfile.worker \
    --build-context "gitlab-cli=$GITLAB_CLI_DIR" \
    --build-context "github-cli=$GITHUB_CLI_DIR" \
    --build-context "sentry-cli=$SENTRY_CLI_DIR" \
    -t "$REGISTRY/claude-agent-worker:$TAG" \
    -t "$REGISTRY/claude-agent-worker:latest" \
    .

echo ""
echo "=== Pushing worker ==="
docker push "$REGISTRY/claude-agent-worker:$TAG"
docker push "$REGISTRY/claude-agent-worker:latest"

# Update ops repo manifests
echo ""
echo "=== Updating ops manifests ==="
OPS_DIR="${OPS_DIR:-$HOME/Projects/globalcomix/ops}"
DEPLOYMENT_FILE="$OPS_DIR/apps/claude-agent/deployment.yaml"

if [[ -f "$DEPLOYMENT_FILE" ]]; then
    # Update server image tag
    sed -i "s|claude-agent-server:[^ ]*|claude-agent-server:$TAG|g" "$DEPLOYMENT_FILE"
    # Update worker image tag in WORKER_IMAGE env
    sed -i "s|claude-agent-worker:[^ \"]*|claude-agent-worker:$TAG|g" "$DEPLOYMENT_FILE"

    echo "Updated $DEPLOYMENT_FILE with tag $TAG"

    # Commit and push ops repo
    echo ""
    echo "=== Committing ops changes ==="
    (
        cd "$OPS_DIR"
        git pull --rebase
        git add apps/claude-agent/deployment.yaml
        git commit -m "claude-agent: update to $TAG" || echo "No changes to commit"
        git push
    )
else
    echo "Warning: $DEPLOYMENT_FILE not found, skipping manifest update"
fi

echo ""
echo "=== Deployment complete ==="
echo "Flux will reconcile the new tags automatically."
kubectl get pods -n "$NAMESPACE"
