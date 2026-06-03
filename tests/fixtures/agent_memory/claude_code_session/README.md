# claude_code_session fixture

This fixture exercises the basic Claude Code transcript import path:

- A human user message (text block)
- An assistant turn with a `Read` tool call and usage tokens
- A tool result for the `Read` call (is_error: false)
- An assistant turn with an `Edit` tool call (produces FileEdit record)
- A tool result for the `Edit` call (is_error: false)
- An assistant turn with a `Bash` `cargo test --all` call (test command)
- A tool result for the `Bash` call (is_error: false, "test result: ok. 5 passed") — produces Verification record
- A final assistant text-only turn ("Tests pass. The edit is complete.") — prose alone must NOT produce Verification (trust boundary test)
