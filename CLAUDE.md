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

### CLI (after building)
```bash
./target/release/claude-agent stats           # Queue statistics
./target/release/claude-agent list --failed   # List failed jobs
./target/release/claude-agent retry <id>      # Retry failed job
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
| `LISTEN_ADDR` | Server listen address | Server (default: 0.0.0.0:8443) |
| `GITLAB_TOKEN` | GitLab API token | Worker |
| `ANTHROPIC_API_KEY` | Anthropic API key | Worker |
| `REVIEW_PAYLOAD` | Base64-encoded job payload | Worker (set by scheduler) |

## Development Workflow

1. Make changes to Rust code
2. Run `cargo test --workspace` to verify
3. Build Docker images if deploying
4. Update image tags in `k8s/deployment.yaml` or `k8s/kustomization.yaml`
5. Commit and push - FluxCD deploys automatically

## Adding New Agent Types

1. Create new file in `crates/agents/src/` (e.g., `sentry_fixer.rs`)
2. Implement `ActionExecutor` trait for environment interactions
3. Define system prompt and tool definitions
4. Add to `crates/agents/src/lib.rs` exports
5. Create corresponding worker mode or extend existing worker

## Testing Webhooks Locally

```bash
# Start Redis
docker run -p 6379:6379 redis:7

# Run server
WEBHOOK_SECRET=test REDIS_URL=redis://localhost:6379 cargo run -p claude-agent-server

# Send test webhook
curl -X POST http://localhost:8443/webhook/gitlab \
  -H "X-Gitlab-Token: test" \
  -H "Content-Type: application/json" \
  -d '{"object_kind":"merge_request",...}'
```

## Security Notes

- Worker runs with `--dangerously-skip-permissions` (isolated in K8s Job)
- NetworkPolicy restricts egress to GitLab/Anthropic APIs only
- Commands in `mr_reviewer.rs` are allowlisted for safety
- Webhook signature verification required
