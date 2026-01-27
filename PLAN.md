# Project Plan

## Current Status: Phase 1 Complete

Core MR review system implemented with:
- Webhook handler for GitLab MR events
- Redis queue for job management
- K8s Job scheduler (sequential execution)
- Claude Code integration
- MR reviewer agent with GitLab API integration
- CLI for queue management
- Network isolation via NetworkPolicy

## Phase 1.5: Production Hardening

### Deployment & Operations
- [ ] Deploy to ops namespace in DOKS cluster
- [ ] Configure Cloudflare tunnel for webhook endpoint
- [ ] Set up sealed secrets for API tokens
- [ ] Add to FluxCD for GitOps deployment
- [ ] Configure GitLab webhook in globalcomix/gc repo

### Monitoring & Alerting
- [ ] Add Prometheus metrics endpoint to server
- [ ] Create Grafana dashboard for queue stats
- [ ] Configure alerts for:
  - Queue backup (>10 pending jobs)
  - High failure rate (>20% in 1 hour)
  - Worker pod failures
  - API rate limiting

### Reliability
- [ ] Add health check that verifies Redis connectivity
- [ ] Implement graceful shutdown for server
- [ ] Add job timeout enforcement in scheduler
- [ ] Retry logic for transient GitLab API failures

### Testing
- [ ] Integration test with mock GitLab webhook
- [ ] End-to-end test with test repository
- [ ] Load test with concurrent webhooks

## Phase 2: GitHub Support

### GitHub Webhook Handler
- [ ] Add `crates/server/src/github.rs` for PR event parsing
- [ ] Support pull_request events (opened, synchronize, reopened)
- [ ] GitHub webhook signature verification (HMAC-SHA256)
- [ ] Route to `/webhook/github` endpoint

### GitHub API Integration
- [ ] Add GitHub client to `crates/agents/`
- [ ] PR review comments API
- [ ] PR approval/request changes API
- [ ] Support GitHub App authentication (JWT + installation tokens)

### Configuration
- [ ] Environment-based provider selection
- [ ] Per-repository provider detection
- [ ] Unified ReviewPayload for both providers

## Phase 3: Sentry Error Handler

Automatically create fix branches for Sentry errors.

### Sentry Integration
- [ ] Add `crates/server/src/sentry.rs` for webhook parsing
- [ ] Support issue.created and issue.regression events
- [ ] Extract stack trace, error message, affected file

### Sentry Fixer Agent
- [ ] Create `crates/agents/src/sentry_fixer.rs`
- [ ] System prompt focused on debugging and fixing
- [ ] Tools: read_file, search_code, create_branch, commit, create_mr
- [ ] Link MR to Sentry issue (Fixes SENTRY-XXX)

### Workflow
```
Sentry Webhook → Queue → Worker → Create Branch → Fix → Create MR
```

### Safety
- [ ] Require human review before merge
- [ ] Limit to specific error types initially
- [ ] Dry-run mode for testing

## Phase 4: Jira Ticket Implementation

Implement features from Jira tickets automatically.

### Jira Integration
- [ ] Add `crates/server/src/jira.rs` for webhook parsing
- [ ] Support issue updated events (status → "Ready for Dev")
- [ ] Extract requirements from description, acceptance criteria

### Jira Worker Agent
- [ ] Create `crates/agents/src/jira_worker.rs`
- [ ] System prompt for feature implementation
- [ ] Tools: read_file, write_file, run_tests, create_branch, commit, create_mr
- [ ] Multi-step planning before implementation

### Workflow
```
Jira Webhook → Queue → Worker → Plan → Implement → Test → Create MR
```

### Guardrails
- [ ] Human approval for plan before implementation
- [ ] Scope limits (max files changed, max lines)
- [ ] Required test coverage for new code

## Phase 5: Enhanced Review Capabilities

### Incremental Reviews
- [ ] Track previous reviews per MR
- [ ] Only review changed files since last review
- [ ] Update existing review comment instead of new one

### Context Awareness
- [ ] Load project-specific review guidelines from `.claude/review.md`
- [ ] Learn from merged MRs (patterns, style)
- [ ] Reference related issues/MRs

### Review Quality
- [ ] Confidence scoring for issues found
- [ ] Categorization (bug, security, performance, style)
- [ ] Severity levels with thresholds

### Inline Comments
- [ ] Post comments on specific lines (GitLab discussions API)
- [ ] Suggest code changes with diff format
- [ ] Batch comments into single review

## Phase 6: Multi-Agent Orchestration

### Specialized Agents
- [ ] Security reviewer (OWASP, dependency vulnerabilities)
- [ ] Performance reviewer (N+1 queries, memory leaks)
- [ ] Documentation reviewer (missing docs, outdated comments)
- [ ] Test coverage reviewer

### Orchestration
- [ ] Parallel agent execution
- [ ] Result aggregation
- [ ] Conflict resolution between agent suggestions

### Agent Selection
- [ ] File-type based routing (PHP → PHP specialist)
- [ ] Label-based agent selection
- [ ] Configurable agent pipeline per repository

## Phase 7: Interactive Reviews

### Conversation Support
- [ ] Respond to review comments/questions
- [ ] Explain reasoning for suggestions
- [ ] Iterate on feedback

### Commands
- [ ] `/claude review` - Trigger manual review
- [ ] `/claude explain <line>` - Explain code section
- [ ] `/claude suggest <description>` - Get implementation suggestion

### MR Updates
- [ ] Re-review on new commits
- [ ] Track addressed vs unaddressed comments
- [ ] Auto-resolve comments when fixed

## Technical Debt & Improvements

### Code Quality
- [ ] Add more unit tests for edge cases
- [ ] Property-based testing for event parsing
- [ ] Benchmark agent loop performance

### Infrastructure
- [ ] Helm chart for easier deployment
- [ ] Support for multiple K8s clusters
- [ ] Database backend option (PostgreSQL) for audit trail

### Developer Experience
- [ ] Local development mode without K8s
- [ ] Mock Claude backend for testing
- [ ] Detailed logging and debugging tools

## Success Metrics

### Phase 1-2 (MR Reviews)
- Reviews posted within 5 minutes of MR update
- <5% false positive rate on blocking issues
- >80% of reviews contain actionable feedback

### Phase 3 (Sentry Fixes)
- >50% of auto-generated fixes pass CI
- >30% of fixes merged without modification
- Mean time to fix reduced by 50%

### Phase 4 (Jira Implementation)
- >30% of simple tickets fully implemented
- >70% of implementations pass initial review
- Developer time saved per ticket: >2 hours

## Timeline (Tentative)

| Phase | Target | Dependencies |
|-------|--------|--------------|
| 1.5 | Week 1-2 | Production access |
| 2 | Week 3-4 | GitHub App setup |
| 3 | Month 2 | Sentry webhook config |
| 4 | Month 3 | Jira webhook config |
| 5 | Month 4 | Phase 1-2 learnings |
| 6 | Month 5-6 | Performance baseline |
| 7 | Month 6+ | User feedback |
