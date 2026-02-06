//! Jira Ticket Handler Agent
//!
//! Analyzes Jira tickets and creates fixes based on the ticket description.

use std::path::Path;

/// System prompt for the Jira ticket handler agent.
pub const JIRA_HANDLER_SYSTEM_PROMPT: &str = r###"You are a developer assistant. A Jira ticket has been assigned to you and your job is to analyze it and implement a fix or feature.

## Instructions

1. **Understand the ticket**: Read the ticket summary, description, and any comments carefully
2. **Explore the codebase**: Use Glob and Grep to find relevant files
3. **Read related code**: Use Read to understand the existing implementation
4. **Implement the change**: Use Edit to make the necessary changes
5. **Test if possible**: If there are relevant tests, run them to verify
6. **Commit and push**: Create a branch, commit the changes, and push

## Creating a Branch and Committing

```bash
# Create a fix branch (already on target branch)
git checkout -b jira-fix/<ISSUE_KEY_LOWERCASE>

# After making changes:
git add -A
git commit -m "<TYPE>: <ISSUE_KEY> - <brief description>

<longer explanation if needed>

Resolves <JIRA_URL>"
git push origin HEAD
```

Where TYPE is one of:
- `fix` - for bug fixes
- `feat` - for new features
- `refactor` - for code refactoring
- `docs` - for documentation changes
- `chore` - for maintenance tasks

## Creating the Merge Request / Pull Request

After pushing, create the MR/PR:

**For GitLab:**
```bash
gitlab mr create -p <PROJECT> --source jira-fix/<ISSUE_KEY_LOWERCASE> --target <TARGET_BRANCH> \
  --title "<TYPE>: <ISSUE_KEY> - <brief description>" \
  --description "## Summary

<what this MR does>

## Changes

- <bullet points of changes>

## Testing

<how to test the changes>

## Jira Ticket

<JIRA_URL>"
```

**For GitHub:**
```bash
gh pr create --title "<TYPE>: <ISSUE_KEY> - <brief description>" \
  --body "## Summary

<what this PR does>

## Changes

- <bullet points of changes>

## Testing

<how to test the changes>

## Jira Ticket

<JIRA_URL>"
```

## Rules

- Focus on what the ticket asks for. Do NOT refactor, improve, or change unrelated code.
- If the ticket is ambiguous, implement the most sensible interpretation and document your assumptions.
- If you cannot complete the task, explain what's blocking you and what investigation is needed.
- Do not add new dependencies unless absolutely necessary.
- Preserve existing code style and patterns.
- Write clear commit messages that explain the "why" not just the "what".

## Do NOT Do

- **Do NOT create or modify database migrations.** Migrations must be created by a human. If the ticket requires a migration, exit with a message explaining what migration is needed.
- **Do NOT modify infrastructure files** (Dockerfiles, CI/CD configs, Kubernetes manifests, deployment scripts).
- **Do NOT change environment variables or secrets.**

## Available Tools

- Read files with the Read tool
- Edit files with the Edit tool
- Search files with Glob and Grep
- Run commands: git, gitlab mr, gh pr, test runners, linters
"###;

/// Context for a Jira ticket job.
#[derive(Debug, Clone)]
pub struct JiraTicketContext {
    /// Jira issue key (e.g., "GC-123")
    pub issue_key: String,
    /// Issue summary/title
    pub summary: String,
    /// Issue description (plain text)
    pub description: Option<String>,
    /// Issue type (e.g., "Bug", "Task", "Story")
    pub issue_type: String,
    /// Priority
    pub priority: Option<String>,
    /// Current status
    pub status: String,
    /// Labels
    pub labels: Vec<String>,
    /// Web URL to the Jira issue
    pub web_url: String,
    /// The comment that triggered this job
    pub trigger_comment: String,
    /// Author of the trigger comment
    pub trigger_author: Option<String>,
    /// VCS project path
    pub vcs_project: String,
    /// Target branch
    pub target_branch: String,
    /// VCS platform (gitlab or github)
    pub vcs_platform: String,
}

/// Jira Ticket Handler Agent.
pub struct JiraHandlerAgent {
    context: JiraTicketContext,
    #[allow(dead_code)]
    repo_path: std::path::PathBuf,
}

impl JiraHandlerAgent {
    pub fn new(context: JiraTicketContext, repo_path: impl AsRef<Path>) -> Self {
        Self {
            context,
            repo_path: repo_path.as_ref().to_path_buf(),
        }
    }

    /// Build the prompt for Claude.
    pub fn build_prompt(&self) -> String {
        let mut prompt = String::new();

        prompt.push_str(JIRA_HANDLER_SYSTEM_PROMPT);
        prompt.push_str("\n\n---\n\n");

        prompt.push_str("## Jira Ticket Details\n\n");
        prompt.push_str(&format!("**Issue Key**: {}\n", self.context.issue_key));
        prompt.push_str(&format!("**Summary**: {}\n", self.context.summary));
        prompt.push_str(&format!("**Type**: {}\n", self.context.issue_type));
        if let Some(ref priority) = self.context.priority {
            prompt.push_str(&format!("**Priority**: {}\n", priority));
        }
        prompt.push_str(&format!("**Status**: {}\n", self.context.status));
        if !self.context.labels.is_empty() {
            prompt.push_str(&format!("**Labels**: {}\n", self.context.labels.join(", ")));
        }
        prompt.push_str(&format!("**URL**: {}\n", self.context.web_url));
        prompt.push_str(&format!("**VCS Project**: {}\n", self.context.vcs_project));
        prompt.push_str(&format!("**Target Branch**: {}\n", self.context.target_branch));
        prompt.push_str(&format!("**VCS Platform**: {}\n", self.context.vcs_platform));

        prompt.push_str("\n## Description\n\n");
        if let Some(ref description) = self.context.description {
            if description.is_empty() {
                prompt.push_str("_No description provided._\n");
            } else {
                prompt.push_str(description);
                prompt.push('\n');
            }
        } else {
            prompt.push_str("_No description provided._\n");
        }

        if !self.context.trigger_comment.is_empty() {
            prompt.push_str("\n## Trigger Comment\n\n");
            if let Some(ref author) = self.context.trigger_author {
                prompt.push_str(&format!("**From**: {}\n\n", author));
            }
            prompt.push_str(&self.context.trigger_comment);
            prompt.push('\n');
        }

        prompt.push_str("\n## Task\n\n");
        prompt.push_str(&format!(
            "1. Analyze the ticket `{}`: {}\n",
            self.context.issue_key, self.context.summary
        ));
        prompt.push_str("2. Explore the codebase to find relevant files\n");
        prompt.push_str("3. Implement the required changes\n");
        prompt.push_str(&format!(
            "4. Create branch `jira-fix/{}` and commit\n",
            self.context.issue_key.to_lowercase()
        ));
        prompt.push_str("5. Push and create an MR/PR\n");

        prompt
    }

    /// Get the system prompt.
    pub fn system_prompt(&self) -> &'static str {
        JIRA_HANDLER_SYSTEM_PROMPT
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_context() -> JiraTicketContext {
        JiraTicketContext {
            issue_key: "GC-123".into(),
            summary: "Fix login button not working on mobile".into(),
            description: Some("The login button on the mobile view doesn't respond to taps. Users report that nothing happens when they tap the button.".into()),
            issue_type: "Bug".into(),
            priority: Some("High".into()),
            status: "In Progress".into(),
            labels: vec!["mobile".into(), "auth".into()],
            web_url: "https://globalcomix.atlassian.net/browse/GC-123".into(),
            trigger_comment: "@claude-agent please fix this bug".into(),
            trigger_author: Some("John Doe".into()),
            vcs_project: "Globalcomix/gc".into(),
            target_branch: "master".into(),
            vcs_platform: "gitlab".into(),
        }
    }

    #[test]
    fn test_build_prompt() {
        let agent = JiraHandlerAgent::new(make_context(), "/tmp/repo");
        let prompt = agent.build_prompt();

        assert!(prompt.contains("GC-123"));
        assert!(prompt.contains("Fix login button not working on mobile"));
        assert!(prompt.contains("Bug"));
        assert!(prompt.contains("High"));
        assert!(prompt.contains("mobile, auth"));
        assert!(prompt.contains("jira-fix/gc-123"));
        assert!(prompt.contains("@claude-agent please fix this bug"));
        assert!(prompt.contains("John Doe"));
    }

    #[test]
    fn test_build_prompt_github() {
        let mut ctx = make_context();
        ctx.vcs_platform = "github".into();

        let agent = JiraHandlerAgent::new(ctx, "/tmp/repo");
        let prompt = agent.build_prompt();

        assert!(prompt.contains("gh pr create"));
    }

    #[test]
    fn test_build_prompt_no_description() {
        let mut ctx = make_context();
        ctx.description = None;

        let agent = JiraHandlerAgent::new(ctx, "/tmp/repo");
        let prompt = agent.build_prompt();

        assert!(prompt.contains("No description provided"));
    }

    #[test]
    fn test_build_prompt_empty_labels() {
        let mut ctx = make_context();
        ctx.labels = vec![];

        let agent = JiraHandlerAgent::new(ctx, "/tmp/repo");
        let prompt = agent.build_prompt();

        // Labels line should not appear
        assert!(!prompt.contains("**Labels**:"));
    }
}
