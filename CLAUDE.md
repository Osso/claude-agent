# CLAUDE.md

Project-specific instructions for Claude Code when working with this repository.

## Essential Commands

### Development
```bash
cargo build --workspace          # Build all crates
cargo test --workspace           # Run all tests
cargo check --workspace          # Quick compile check
cargo clippy --workspace         # Lint
./run-tests.sh                   # Run tests + clippy
```

### Docker
```bash
docker build -f Dockerfile.server -t claude-agent-server:latest .
docker build -f Dockerfile.worker -t claude-agent-worker:latest .
```

### Kubernetes
```bash
kubectl apply -k k8s/            # Deploy all resources
kubectl get pods -n claude-agent # Check pod status
kubectl logs -n claude-agent -l app=claude-agent-server -f  # Server logs
```

### CLI (symlinked to ~/.local/bin/claude-agent)
```bash
claude-agent info -p Globalcomix/gc -m <iid> --token "$TOKEN"  # Show MR info
claude-agent review -p Globalcomix/gc -m <iid> --token "$TOKEN"  # Queue review
claude-agent stats                            # Queue statistics
claude-agent list-failed                      # List failed items
claude-agent jobs [-a]                        # List K8s jobs
claude-agent logs [job-name] [-f] [-n 100]    # Show job logs
claude-agent retry <id>                       # Retry failed item
```

## Architecture

```
GitLab Webhook → Server → Redis Queue → Scheduler → K8s Job (Worker)
                                                         ↓
                                                    Claude Code CLI
                                                         ↓
                                                    GitLab API
```

### Crates

| Crate | Purpose |
|-------|---------|
| `core` | Agent loop, event types, state management |
| `claude` | Claude Code CLI integration (spawn, parse output) |
| `agents` | Agent implementations (MR reviewer) |
| `server` | Webhook handler, Redis queue, K8s scheduler |
| `worker` | Ephemeral K8s job entry point |
| `cli` | Queue management CLI |

## Key Files

- `crates/agents/src/mr_reviewer.rs` - MR review system prompt and tool definitions
- `crates/server/src/scheduler.rs` - K8s Job spawning logic
- `crates/server/src/gitlab.rs` - GitLab webhook event parsing
- `k8s/network-policy.yaml` - Network isolation rules

## Configuration

| Environment Variable | Description | Required |
|---------------------|-------------|----------|
| `REDIS_URL` | Redis connection URL | Server |
| `WEBHOOK_SECRET` | GitLab webhook token | Server |
| `API_KEY` | API key for CLI access (defaults to WEBHOOK_SECRET) | Server (optional) |
| `LISTEN_ADDR` | Server listen address | Server (default: 0.0.0.0:8443) |
| `GITLAB_TOKEN` | GitLab API token | Worker |
| `ANTHROPIC_API_KEY` | Anthropic API key | Worker |
| `REVIEW_PAYLOAD` | Base64-encoded job payload | Worker (set by scheduler) |

### CLI Configuration

The CLI reads configuration from `~/.config/claude-agent/config.toml`:

```toml
server_url = "https://claude-agent.globalcomixdev.com"
api_key = "your-api-key-here"
```

Environment variables override config file values:

| Environment Variable | Description |
|---------------------|-------------|
| `CLAUDE_AGENT_URL` | Server URL for HTTP API access |
| `CLAUDE_AGENT_API_KEY` | API key for authenticating to the server |

## Deployment

### Using deploy.sh (recommended)
```bash
./deploy.sh              # Deploy with :latest tag
./deploy.sh 2026.01.27   # Deploy with specific date tag
```

This builds and pushes both server and worker images, then restarts the deployment.

### Manual Deployment
```bash
# Build and push server
docker build -f Dockerfile.server -t registry.digitalocean.com/globalcomix/claude-agent-server:latest .
docker push registry.digitalocean.com/globalcomix/claude-agent-server:latest

# Build and push worker
docker build -f Dockerfile.worker -t registry.digitalocean.com/globalcomix/claude-agent-worker:latest .
docker push registry.digitalocean.com/globalcomix/claude-agent-worker:latest

# Restart deployment
kubectl rollout restart deployment/claude-agent-server -n claude-agent
kubectl rollout status deployment/claude-agent-server -n claude-agent
```

## Development Workflow

1. Make changes to Rust code
2. Run `cargo test --workspace` to verify
3. Build Docker images if deploying
4. Run `./deploy.sh` to deploy

## Adding New Agent Types

1. Create new file in `crates/agents/src/` (e.g., `sentry_fixer.rs`)
2. Implement `ActionExecutor` trait for environment interactions
3. Define system prompt and tool definitions
4. Add to `crates/agents/src/lib.rs` exports
5. Create corresponding worker mode or extend existing worker

## Testing Webhooks Locally

```bash
# Start server + valkey
docker compose up -d

# Send test webhook
curl -X POST http://localhost:8443/webhook/gitlab \
  -H "X-Gitlab-Token: test" \
  -H "Content-Type: application/json" \
  -d '{"object_kind":"merge_request",...}'
```

## Security Notes

- Worker runs with `--dangerously-skip-permissions` (isolated in K8s Job with no persistent storage)
- NetworkPolicy restricts egress to external HTTPS only (no private network access)
- Webhook signature verification required (`X-Gitlab-Token` header)
- API endpoints protected by API key (`Authorization: Bearer <key>` header)
