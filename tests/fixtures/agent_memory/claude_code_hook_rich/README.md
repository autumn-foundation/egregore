# claude_code_hook_rich fixture

This fixture exercises hook events, failure paths, and mixed tool kinds:

- A human user message (text block)
- A `PreToolUse` hook for Bash (produces Diagnostic record)
- An assistant turn with a `Bash` `cargo test --all` tool call (test command, success) — produces Verification record
- A `PostToolUse` hook for Bash (produces Diagnostic record)
- A tool result for the test Bash call (is_error: false)
- An assistant turn with a `Bash` `git push origin main` tool call (non-test, non-patch)
- A tool result for the git-push call (is_error: true, "Permission denied") — produces Failure record
- An assistant turn with an `Agent` subagent tool call (unknown tool kind → "other")
- A tool result for the Agent call (is_error: false)
- A `Stop` hook event (produces Diagnostic record)
