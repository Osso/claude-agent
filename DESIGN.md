# Design Document

## Overview

Claude Agent is an agentic system that automates code review by receiving webhooks from GitLab, queuing review jobs, and executing Claude Code in isolated Kubernetes Jobs to analyze merge requests and post feedback.

## Goals

1. **Automated MR Reviews**: Reduce review burden by providing initial feedback on code changes
2. **Security Isolation**: Run untrusted code analysis in ephemeral, network-isolated containers
3. **Extensibility**: Support future agent types (Sentry error fixing, Jira ticket implementation)
4. **Reliability**: Queue-based architecture with failure handling and retry capability

## Non-Goals

- Real-time streaming of review progress to users
- Complex multi-agent orchestration (single agent per job)
- Self-hosting Claude models (uses Anthropic API via Claude Code CLI)

## Architecture

### System Components

```
┌─────────────────────────────────────────────────────────────┐
│                    Kubernetes Cluster                        │
│                     (claude-agent namespace)                 │
│                                                             │
│  ┌─────────────┐     ┌──────────────┐     ┌─────────────┐  │
│  │   Webhook   │────▶│    Redis     │◀────│  Scheduler  │  │
│  │   Handler   │     │    Queue     │     │             │  │
│  │   (axum)    │     │  (Dragonfly) │     │             │  │
│  └─────────────┘     └──────────────┘     └──────┬──────┘  │
│         ▲                                        │         │
│         │                                        ▼         │
│  ┌──────┴──────┐                    ┌────────────────────┐ │
│  │ Cloudflared │                    │   Kubernetes Job   │ │
│  │   Tunnel    │                    │  ┌──────────────┐  │ │
│  └─────────────┘                    │  │    Worker    │  │ │
│                                     │  │              │  │ │
│                                     │  │ Claude Code  │  │ │
│                                     │  │    CLI       │  │ │
│                                     │  └──────┬───────┘  │ │
│                                     └─────────┼──────────┘ │
│                                               │            │
└───────────────────────────────────────────────┼────────────┘
                                                │
                                    ┌───────────▼───────────┐
                                    │     GitLab API        │
                                    │  (clone, post review) │
                                    └───────────────────────┘
```

### Component Responsibilities

#### Webhook Handler (Server)
- Receives GitLab MR webhooks via Cloudflare tunnel
- Validates webhook signature
- Filters events (only open, non-draft MRs on open/update/reopen)
- Enqueues review payloads to Redis

#### Redis Queue
- Lightweight job persistence
- Stores: pending queue, processing set, failed list
- Uses existing Dragonfly instance in globalcomix namespace

#### Scheduler
- Polls Redis queue for pending jobs
- Spawns Kubernetes Jobs sequentially (one at a time)
- Monitors job completion/failure
- Updates queue state accordingly

#### Worker (K8s Job)
- Ephemeral container with Claude Code CLI installed
- Clones repository, checks out source branch
- Runs agent loop with Claude Code
- Posts review comments via GitLab API
- Auto-deleted after completion (TTL: 1 hour)

#### Agent Loop (Core)
- Event-driven architecture with Action/Observation pattern
- Maintains conversation history and state
- Executes tools: read_file, run_command, post_comment, approve, request_changes
- Terminates on finish action or max iterations

## Design Decisions

### 1. Sequential Job Processing

**Decision**: Process one review at a time instead of parallel execution.

**Rationale**:
- Simpler debugging and monitoring
- Predictable resource usage
- Avoids API rate limiting issues
- Reviews are not time-critical (minutes acceptable)

**Trade-off**: Lower throughput, but sufficient for expected volume.

### 2. Ephemeral Kubernetes Jobs

**Decision**: Each review runs in a fresh K8s Job that's deleted after completion.

**Rationale**:
- Complete isolation between reviews
- No state leakage between projects
- Easy resource cleanup
- Natural timeout handling via Job spec

**Alternative considered**: Long-running worker pods with sandboxed execution. Rejected due to complexity of maintaining isolation.

### 3. Claude Code CLI Integration

**Decision**: Use Claude Code CLI with stream-json format instead of direct API calls.

**Rationale**:
- Leverages Claude Code's tool infrastructure
- Automatic prompt caching and optimization
- Consistent behavior with interactive Claude Code
- Simpler than reimplementing tool execution

**Trade-off**: Dependency on Claude Code installation in worker image.

### 4. Redis for Queue (Dragonfly)

**Decision**: Use existing Dragonfly (Redis-compatible) for job queue.

**Rationale**:
- Already deployed and managed
- Simple LPUSH/BRPOP operations sufficient
- No need for dedicated message broker
- Lightweight persistence for job recovery

### 5. Network Isolation

**Decision**: Strict NetworkPolicy limiting egress to GitLab and Anthropic APIs only.

**Rationale**:
- Worker executes untrusted code (repository content)
- Prevents data exfiltration
- Limits blast radius of potential exploits

### 6. Command Allowlisting

**Decision**: Worker only allows specific safe commands (cargo test, npm test, linters).

**Rationale**:
- Prevents arbitrary code execution attacks via MR content
- Agent can still run useful analysis tools
- Easy to extend allowlist as needed

## Data Flow

### Webhook to Queue

```
1. GitLab sends MR event to /webhook/gitlab
2. Handler validates X-Gitlab-Token header
3. Handler parses MergeRequestEvent
4. Handler checks should_review() criteria
5. Handler creates ReviewPayload from event
6. Handler pushes to Redis queue (RPUSH)
7. Handler returns 202 Accepted with job_id
```

### Queue to Review

```
1. Scheduler polls queue (BLPOP with timeout)
2. Scheduler creates K8s Job with payload in env var
3. Worker starts, decodes payload
4. Worker clones repository
5. Worker spawns Claude Code process
6. Worker builds initial prompt with diff
7. Agent loop: prompt → response → tool execution → repeat
8. Agent calls finish() with ReviewResult
9. Worker exits, Job completes
10. Scheduler marks job completed in queue
```

### Agent Loop

```
while running:
    messages = build_messages_from_history()
    response = claude.prompt(messages)

    for item in response:
        if item is ToolUse:
            action = parse_action(item)
            if action is Finish:
                return result
            observation = executor.execute(action)
            history.add(observation)
        elif item is Text:
            history.add(message)
```

## Security Model

### Trust Boundaries

1. **Webhook Handler**: Trusts GitLab (via webhook secret)
2. **Queue**: Internal, trusted
3. **Worker**: Untrusted environment (executes repo code)
4. **GitLab API**: Authenticated via token

### Threat Mitigations

| Threat | Mitigation |
|--------|------------|
| Malicious MR content | Command allowlist, network isolation |
| Webhook spoofing | Token verification |
| Data exfiltration | NetworkPolicy egress rules |
| Resource exhaustion | Job resource limits, max iterations |
| Token theft | Secrets mounted as env vars, not files |

## Error Handling

### Transient Failures
- Redis connection: Retry with backoff
- GitLab API: Retry within agent loop
- K8s Job spawn: Logged, job marked failed

### Permanent Failures
- Invalid webhook: 400 response, not queued
- Job timeout: Marked failed, moved to failed queue
- Max iterations: Marked failed with reason

### Recovery
- Failed jobs stored in Redis list
- CLI can list and retry failed jobs
- Manual intervention for persistent failures

## Observability

### Logging
- Structured JSON logs (tracing-subscriber)
- Request IDs for correlation
- Agent iteration tracking

### Metrics (Future)
- Queue depth
- Job duration histogram
- Success/failure rates
- API token usage

### Alerting (Future)
- Queue backup threshold
- Job failure rate
- Worker pod failures

## Capacity Planning

### Expected Load
- ~10-50 MRs per day
- Average review: 2-5 minutes
- Sequential processing: ~2-4 hours capacity per day

### Resource Estimates
- Server: 128Mi-256Mi RAM, minimal CPU
- Worker: 512Mi-4Gi RAM (Claude Code), 0.5-2 CPU
- Redis: Negligible additional load

## Future Considerations

### Scaling
- Multiple scheduler instances with distributed locking
- Parallel job execution with rate limiting
- Dedicated worker node pool

### Multi-Cluster
- Central queue, multi-cluster workers
- Region-aware job routing

### Audit Trail
- Store all reviews in persistent storage
- Link reviews to MR history
- Compliance reporting
