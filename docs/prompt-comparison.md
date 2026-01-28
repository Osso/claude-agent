# MR Review Prompt Comparison

Comparison of our review prompt against Qodo PR-Agent and OpenHands.

## Our Prompt (claude-agent)

**Approach**: Free-form natural language review. The agent gets a system prompt with guidelines, the diff, and MR metadata. Produces inline comments at specific lines plus general comment.

**Strengths**:
- Clear "do NOT focus on" list (style, preferences, hypotheticals) reduces noise
- Explicit 4-category focus: bugs, security, performance, code quality
- Incremental update review flow with unresolved thread tracking
- Auto-approval path when MR looks good
- Line-level inline comments with SHA-based positioning (GitLab) and review API (GitHub)
- Lint-fix pipeline: CI failure triggers auto-fix job that reads linter output and pushes fixes

**Weaknesses**:
- No structured output — review quality depends entirely on the LLM
- No scoring or effort estimation
- No ticket/issue compliance tracking
- No "can this PR be split" analysis
- No TODO scanning
- No test coverage check (yes/no: does this PR include tests?)

## Qodo PR-Agent

**Source**: [pr_reviewer_prompts.toml](https://github.com/qodo-ai/pr-agent/blob/main/pr_agent/settings/pr_reviewer_prompts.toml)

**Approach**: Structured YAML output via Pydantic schema. The LLM returns machine-parseable YAML that the tool renders into comments, labels, etc.

**Strengths**:
- Structured output with typed fields: `key_issues_to_review` includes `relevant_file`, `start_line`, `end_line`, `issue_header`, `issue_content`
- PR scoring (0-100) and effort estimation (1-5)
- Security review as explicit field with category examples (SQL injection, XSS, etc.)
- Test coverage check (yes/no)
- TODO scanning across the diff
- Ticket compliance: checks PR against linked ticket requirements (compliant / not compliant / needs human verification)
- PR splittability analysis: suggests independent sub-PRs
- Contribution time cost estimation (best/average/worst case)
- Extra instructions injection point for per-repo customization
- Explicit diff format documentation (explains `+`, `-`, ` ` notation)
- AI-generated summary metadata support

**Weaknesses**:
- Complex Jinja2 template with many conditionals
- YAML-only output is brittle (parsing failures)
- No incremental/update review concept — each review is full
- No auto-fix capability
- Over-engineered for simpler use cases

## OpenHands

**Source**: [prompt.py](https://github.com/OpenHands/software-agent-sdk/blob/main/examples/03_github_workflows/02_pr_review/prompt.py)

**Approach**: Minimal template that delegates to skill triggers (`/codereview`, `/github-pr-review`).

**Strengths**:
- Simple, composable via skill system
- Includes commit ID for precise referencing

**Weaknesses**:
- The public prompt is essentially empty — just "Review the PR changes below and identify issues"
- No review guidelines, categories, or structured output
- No diff format explanation
- No incremental review
- Real logic is hidden behind proprietary skills
- Not useful as a standalone prompt comparison

## Feature Matrix

| Feature | claude-agent | Qodo PR-Agent | OpenHands |
|---------|:---:|:---:|:---:|
| Inline comments (file:line) | Yes | Yes | Unknown |
| General comments | Yes | Yes | Yes |
| Incremental review (updates) | Yes | No | No |
| Auto-approval | Yes | No | No |
| Structured output | No | Yes (YAML) | No |
| PR scoring | No | Yes (0-100) | No |
| Effort estimation | No | Yes (1-5) | No |
| Security review | Guidelines | Explicit field | No |
| Test coverage check | No | Yes | No |
| TODO scanning | No | Yes | No |
| Ticket compliance | No | Yes | No |
| PR splittability | No | Yes | No |
| Auto-fix linting | Yes | No | No |
| Custom instructions hook | No | Yes | No |
| Diff format docs | No | Yes | No |

## Potential Improvements

Based on this comparison, features worth adding:
1. **Test coverage check** — simple yes/no field
2. **Custom per-project instructions** — extra_instructions in the prompt
3. **Diff format documentation** — explicit explanation of unified diff notation
4. **Security as structured output** — dedicated section rather than relying on LLM judgment
