//! Sentry Issue Fixer Agent
//!
//! Analyzes Sentry errors and attempts to fix them.

use std::path::Path;

/// System prompt for the Sentry fixer agent.
pub const SENTRY_FIX_SYSTEM_PROMPT: &str = r###"You are a code fixer. A Sentry error has been reported and your job is to analyze and fix it.

## Instructions

1. **Understand the error**: Read the stacktrace carefully to identify the root cause
2. **Locate the code**: Use the Read tool to examine the files mentioned in the stacktrace
3. **Implement the fix**: Use the Edit tool to fix the bug
4. **Test if possible**: If there are relevant tests, run them to verify the fix
5. **Commit and push**: Create a branch, commit the fix, and push

## Creating a Branch and Committing

```bash
# Create a fix branch (already on target branch)
git checkout -b sentry-fix/<SHORT_ID>

# After making changes:
git add -A
git commit -m "fix: <SHORT_ID> - <brief description>

Resolves <SENTRY_URL>"
git push origin HEAD
```

## Creating the Pull Request

After pushing, create the PR:

```bash
gh pr create --title "fix: <SHORT_ID> - <brief description>" \
  --body "## Summary

Fixes Sentry issue <SHORT_ID>: <ERROR_TITLE>

## Root Cause

<explain what caused the error>

## Fix

<explain what you changed>

## Sentry Issue

<SENTRY_URL>"
```

## Project Guidelines

Before making changes, check if `.claude/mr.md` exists in the repo root. If it does, read it and follow those project-specific guidelines for code changes and PR creation.

## Rules

- Only fix the specific error reported. Do NOT refactor, improve, or change other code.
- If the fix requires significant design decisions, explain the options and pick the safest one.
- If you cannot determine a fix, explain what investigation is needed and create a PR with your analysis.
- Do not add new dependencies unless absolutely necessary.
- Preserve existing code style and patterns.
- Do NOT add self-review comments to PRs. Do not comment "LGTM" on your own PRs.

## CRITICAL: Never Suppress Error Reporting

**NEVER remove, skip, or conditionally suppress `log_exception_in_sentry()` calls or similar error reporting.** Removing error reporting is destructive — it makes production issues invisible.

If an error is expected (e.g., user input validation, known edge cases), **downgrade its severity** instead of removing the log:

```php
// WRONG — suppresses error reporting entirely
if (!$isExpectedError) {
    log_exception_in_sentry($e);
}

// CORRECT — logs at info level so it's still visible but doesn't alert
log_exception_in_sentry($e, level: $isExpectedError ? \Sentry\Severity::info() : null);
```

The `log_exception_in_sentry()` function accepts a `$level` parameter. Use `\Sentry\Severity::info()` for expected errors. This keeps errors visible in Sentry for monitoring while preventing alert noise.

Similarly, never move errors from info-level lists to expected/ignored lists. If an error is already logged at info level, that's the correct handling.

**Also never:** add errors to "expected errors" documentation tables as a substitute for a code fix.

## Do NOT Fix

Some errors cannot be fixed with code changes alone. If the error falls into any of these categories, do NOT create a branch or PR. Instead, exit with a message explaining why you cannot fix it:

- **Missing database migrations**: Errors caused by missing columns, tables, or schema changes. These require a migration to be created and deployed by a human. Do not attempt workarounds like commenting out code that references the missing column.
- **Infrastructure/deployment issues**: Errors caused by deployment timing, missing environment variables, misconfigured services, or DNS problems.
- **Data issues**: Errors caused by corrupt or unexpected data that needs manual cleanup.
- **Third-party service outages**: Errors caused by external APIs being down or returning unexpected responses temporarily.
- **Rate limiting or resource exhaustion**: Errors caused by hitting API limits, running out of disk space, or memory issues.

## Available Tools

- Read files with the Read tool
- Edit files with the Edit tool
- Run commands: git, gh pr, test runners, linters
"###;

/// Context for a Sentry fix job.
#[derive(Debug, Clone)]
pub struct SentryFixContext {
    pub short_id: String,
    pub title: String,
    pub culprit: String,
    pub platform: String,
    pub web_url: String,
    pub stacktrace: String,
    pub tags: Vec<(String, String)>,
    pub vcs_project: String,
    pub target_branch: String,
    pub vcs_platform: String,
}

/// Sentry Issue Fixer Agent.
pub struct SentryFixerAgent {
    context: SentryFixContext,
    #[allow(dead_code)]
    repo_path: std::path::PathBuf,
}

impl SentryFixerAgent {
    pub fn new(context: SentryFixContext, repo_path: impl AsRef<Path>) -> Self {
        Self {
            context,
            repo_path: repo_path.as_ref().to_path_buf(),
        }
    }

    pub fn build_prompt(&self) -> String {
        let mut prompt = String::new();
        prompt.push_str(SENTRY_FIX_SYSTEM_PROMPT);
        prompt.push_str("\n\n---\n\n");
        self.append_issue_details(&mut prompt);
        self.append_error_details(&mut prompt);
        self.append_task(&mut prompt);
        prompt
    }

    pub fn system_prompt(&self) -> &'static str {
        SENTRY_FIX_SYSTEM_PROMPT
    }

    fn append_issue_details(&self, prompt: &mut String) {
        prompt.push_str("## Sentry Issue Details\n\n");
        prompt.push_str(&format!("**Short ID**: {}\n", self.context.short_id));
        prompt.push_str(&format!("**Title**: {}\n", self.context.title));
        prompt.push_str(&format!("**Location**: {}\n", self.context.culprit));
        prompt.push_str(&format!("**Platform**: {}\n", self.context.platform));
        prompt.push_str(&format!("**URL**: {}\n", self.context.web_url));
        prompt.push_str(&format!("**VCS Project**: {}\n", self.context.vcs_project));
        prompt.push_str(&format!("**Target Branch**: {}\n", self.context.target_branch));
        prompt.push_str(&format!("**VCS Platform**: {}\n", self.context.vcs_platform));

        if !self.context.tags.is_empty() {
            prompt.push_str("\n**Tags**:\n");
            for (key, value) in &self.context.tags {
                prompt.push_str(&format!("- {}: {}\n", key, value));
            }
        }
    }

    fn append_error_details(&self, prompt: &mut String) {
        prompt.push_str("\n## Error Details\n\n");
        if self.context.stacktrace.is_empty() {
            prompt.push_str("_No stacktrace available. Investigate based on the culprit location._\n");
        } else {
            prompt.push_str(&self.context.stacktrace);
        }
    }

    fn append_task(&self, prompt: &mut String) {
        prompt.push_str("\n\n## Task\n\n");
        prompt.push_str(&format!("1. Analyze the error in `{}`\n", self.context.culprit));
        prompt.push_str("2. Read the relevant source files to understand the context\n");
        prompt.push_str("3. Implement a fix for the root cause\n");
        prompt.push_str(&format!(
            "4. Create branch `sentry-fix/{}` and commit the fix\n",
            self.context.short_id.to_lowercase()
        ));
        prompt.push_str("5. Push and create a PR\n");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_context() -> SentryFixContext {
        SentryFixContext {
            short_id: "WEB-123".into(),
            title: "NullPointerException in FooService".into(),
            culprit: "app/Services/FooService.php in doSomething".into(),
            platform: "php".into(),
            web_url: "https://sentry.io/issues/12345".into(),
            stacktrace: "## NullPointerException\n\ndoSomething in FooService.php:42\n".into(),
            tags: vec![
                ("environment".into(), "production".into()),
                ("browser".into(), "Chrome".into()),
            ],
            vcs_project: "globalcomix/gc".into(),
            target_branch: "master".into(),
            vcs_platform: "github".into(),
        }
    }

    #[test]
    fn test_build_prompt() {
        let agent = SentryFixerAgent::new(make_context(), "/tmp/repo");
        let prompt = agent.build_prompt();

        assert!(prompt.contains("WEB-123"));
        assert!(prompt.contains("NullPointerException"));
        assert!(prompt.contains("FooService.php"));
        assert!(prompt.contains("sentry-fix/web-123"));
        assert!(prompt.contains("gh pr create"));
        assert!(!prompt.contains("gitlab mr create"));
        assert!(prompt.contains("environment: production"));
        assert!(prompt.contains("Never Suppress Error Reporting"));
        assert!(prompt.contains("Severity::info()"));
    }

    #[test]
    fn test_build_prompt_github() {
        let agent = SentryFixerAgent::new(make_context(), "/tmp/repo");
        let prompt = agent.build_prompt();

        assert!(prompt.contains("gh pr create"));
    }

    #[test]
    fn test_build_prompt_no_stacktrace() {
        let mut ctx = make_context();
        ctx.stacktrace = String::new();

        let agent = SentryFixerAgent::new(ctx, "/tmp/repo");
        let prompt = agent.build_prompt();

        assert!(prompt.contains("No stacktrace available"));
    }
}
