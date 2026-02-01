#!/usr/bin/env bash
set -euo pipefail

REGISTRY="registry.digitalocean.com/globalcomix"
BASE_TAG="${1:-$(date +%Y.%m.%d)}"
NAMESPACE="claude-agent"

# Check if a tag exists in the registry
tag_exists() {
    local tag="$1"
    docker manifest inspect "$REGISTRY/claude-agent-server:$tag" &>/dev/null
}

# Find next available tag (auto-increment suffix if base tag exists)
find_next_tag() {
    local base="$1"

    if ! tag_exists "$base"; then
        echo "$base"
        return
    fi

    local suffix=1
    while tag_exists "$base.$suffix"; do
        ((suffix++))
    done
    echo "$base.$suffix"
}

TAG=$(find_next_tag "$BASE_TAG")

echo "=== Deploying claude-agent (tag: $TAG) ==="

# Update VERSION constants in source files
update_version() {
    local file="$1"
    local current
    current=$(grep -oP 'const VERSION: &str = "\K[^"]+' "$file" || echo "")
    sed -i "s|const VERSION: &str = \"[^\"]*\"|const VERSION: \&str = \"$TAG\"|" "$file"
    echo "Updated $file: $current -> $TAG"
}

echo ""
echo "=== Updating VERSION constants ==="
update_version "crates/server/src/main.rs"
update_version "crates/worker/src/main.rs"

# Build images
./build.sh "$TAG"

# Push server
echo ""
echo "=== Pushing server ==="
docker push "$REGISTRY/claude-agent-server:$TAG"
docker push "$REGISTRY/claude-agent-server:latest"

# Push worker
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
    # Pull ops repo first to avoid rebase conflicts
    echo ""
    echo "=== Pulling ops repo ==="
    (cd "$OPS_DIR" && git pull --rebase)

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
        git add apps/claude-agent/deployment.yaml
        git commit -m "claude-agent: update to $TAG" || echo "No changes to commit"
        git push
    )

    # Reconcile Flux to apply changes immediately
    echo ""
    echo "=== Reconciling Flux ==="
    (cd "$OPS_DIR" && ./scripts/reconcile-flux.sh apps)
else
    echo "Warning: $DEPLOYMENT_FILE not found, skipping manifest update"
fi

echo ""
echo "=== Deployment complete ==="
kubectl get pods -n "$NAMESPACE"
