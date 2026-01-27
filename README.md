# Claude Agent

Agentic system that receives webhooks from GitLab to auto-review MRs using Claude Code.

**Documentation**:
- [CLAUDE.md](CLAUDE.md) - Development commands and project structure
- [DESIGN.md](DESIGN.md) - Architecture and design decisions
- [PLAN.md](PLAN.md) - Roadmap and future features

## Architecture

```
GitLab Webhook → Server → Redis Queue → Scheduler → K8s Job (Worker)
                                                         ↓
                                                    Claude Code
                                                         ↓
                                                    GitLab API
```

## Components

- **Server**: Receives GitLab webhooks, queues review jobs
- **Worker**: Runs Claude Code agent for MR review in ephemeral K8s Jobs
- **CLI**: Queue management and manual testing

## Building

```bash
cargo build --release
```

## Configuration

Environment variables:

| Variable | Description | Default |
|----------|-------------|---------|
| `REDIS_URL` | Redis connection URL | `redis://127.0.0.1:6379` |
| `WEBHOOK_SECRET` | GitLab webhook token | (required) |
| `LISTEN_ADDR` | Server listen address | `0.0.0.0:8443` |
| `GITLAB_TOKEN` | GitLab API token (worker) | (required) |
| `ANTHROPIC_API_KEY` | Anthropic API key (worker) | (required) |

## Deployment

See `k8s/` directory for Kubernetes manifests.

```bash
kubectl apply -k k8s/
```

## CLI Usage

```bash
# Show queue stats
claude-agent stats

# List failed items
claude-agent list --failed

# Retry a failed job
claude-agent retry <job-id>

# Clear failed items
claude-agent clear-failed

# Manually queue a review
claude-agent queue \
  --gitlab-url https://gitlab.com \
  --project group/project \
  --mr-iid 123 \
  --clone-url https://gitlab.com/group/project.git \
  --source-branch feature \
  --title "My MR" \
  --author username
```

## GitLab Webhook Setup

1. Create a webhook in GitLab project settings
2. URL: `https://your-domain/webhook/gitlab`
3. Secret Token: Match `WEBHOOK_SECRET` env var
4. Trigger: Merge request events
5. SSL verification: Enable

## License

MIT
