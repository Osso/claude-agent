# Claude Agent

Agentic system that receives webhooks from GitLab to auto-review MRs using GPT through the OpenAI API.

**Documentation**:
- [CLAUDE.md](CLAUDE.md) - Development commands and project structure
- [DESIGN.md](DESIGN.md) - Architecture and design decisions
- [PLAN.md](PLAN.md) - Roadmap and future features

## Architecture

```
GitLab Webhook → Server → Redis Queue → Scheduler → K8s Job (Worker)
                                                         ↓
                                                    GPT API worker
                                                         ↓
                                                    GitLab API
```

## Components

- **Server**: Receives GitLab webhooks, queues review jobs
- **Worker**: Runs a GPT agent for MR review in ephemeral K8s Jobs
- **CLI**: Queue management and manual testing

## Building

```bash
cargo build --release
```

### Docker CA trust

Both Dockerfiles pin Bookworm `ca-certificates=20250419~deb12u2` before Cargo/Go network operations and in runtime stages. Worker Rust builders share the same Rust 1.92 CA stage; Jira stays on Go 1.24. Mago stays at 1.2.2, built from its SHA-256-pinned original public crate with only `webpki-roots` 1.0.4 → 1.0.10 and `rustls-webpki` 0.103.8 → 0.103.16 updated before locked installation. GitHub/Sentry CLI Git installs use `--locked`; their canonical sources commit the same corrected CA dependencies (`github-cli` `44896c0`, `sentry-cli` `9154e6c`). Existing Git HEAD selection policy is unchanged.

CA-only runtime promotion uses Ops derivative images, not this project's `deploy.sh`, which rebuilds application binaries.

## Configuration

Environment variables:

| Variable | Description | Default |
|----------|-------------|---------|
| `REDIS_URL` | Redis connection URL | `redis://127.0.0.1:6379` |
| `WEBHOOK_SECRET` | GitLab webhook token | (required) |
| `LISTEN_ADDR` | Server listen address | `0.0.0.0:8443` |
| `GITLAB_TOKEN` | GitLab API token (worker) | (required) |
| `OPENAI_API_KEY` | OpenAI API key (worker) | (required) |
| `OPENAI_MODEL` | OpenAI model for worker GPT calls | `gpt-5.4-mini` |
| `OPENAI_API_BASE` | OpenAI API base URL | `https://api.openai.com/v1` |
| `CLAUDE_CODE_OAUTH_TOKEN` | Legacy Claude OAuth token kept during migration | (optional) |

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
