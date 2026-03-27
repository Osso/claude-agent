#!/usr/bin/env bash
set -euo pipefail

# Test Sentry fix job locally

ISSUE_ID="7215158142"
SHORT_ID="WEB-818"

# Create minimal payload - worker will fetch details from Sentry API
PAYLOAD=$(cat <<PAYLOAD_EOF
{
  "type": "sentry_fix",
  "issue_id": "$ISSUE_ID",
  "short_id": "$SHORT_ID",
  "title": "",
  "culprit": "",
  "platform": "",
  "issue_type": "error",
  "issue_category": "error",
  "web_url": "https://globalcomix.sentry.io/issues/$ISSUE_ID/",
  "project_slug": "web",
  "organization": "globalcomix",
  "clone_url": "https://gitlab.com/Globalcomix/gc.git",
  "target_branch": "master",
  "vcs_platform": "gitlab",
  "vcs_project": "Globalcomix/gc"
}
PAYLOAD_EOF
)

PAYLOAD_B64=$(echo "$PAYLOAD" | base64 -w0)

echo "Testing Sentry fix job for $SHORT_ID"
echo "Payload (first 100 chars): ${PAYLOAD_B64:0:100}..."
echo

# Export required env vars
export REVIEW_PAYLOAD="$PAYLOAD_B64"
export GITLAB_TOKEN="${GITLAB_TOKEN:-}"
export SENTRY_AUTH_TOKEN="${SENTRY_AUTH_TOKEN:-}"
export ANTHROPIC_API_KEY="${ANTHROPIC_API_KEY:-}"

# Check required vars
if [[ -z "$GITLAB_TOKEN" ]]; then
    echo "Error: GITLAB_TOKEN not set"
    exit 1
fi
if [[ -z "$SENTRY_AUTH_TOKEN" ]]; then
    echo "Error: SENTRY_AUTH_TOKEN not set"
    exit 1
fi

# Create work directory
WORK_DIR=$(mktemp -d)
echo "Work directory: $WORK_DIR"

# Run worker (it expects /work to exist)
mkdir -p "$WORK_DIR/work"
cd "$WORK_DIR"
ln -s "$WORK_DIR/work" /tmp/claude-sentry-test-work 2>/dev/null || true

echo "Running worker..."
cargo run --release -p claude-agent-worker
