//! Classify tool failures into recurring, fixable categories. The value is not
//! the raw error count but the *pattern*: 100+ "string to replace not found"
//! across sessions is fixable friction (read-before-edit), not noise. Lexical
//! heuristics over the error text — the fallback for everything the transcript
//! does not label itself; reports say which is which.

/// A category of tool failure, ordered so the most specific match wins.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorCategory {
    /// Edit/Write precondition: stale read, file changed, target string absent.
    EditPrecondition,
    /// A command or operation blocked by a hook or permission rule.
    BlockedByHook,
    /// A command binary was not found.
    CommandNotFound,
    /// A file or path that does not exist.
    PathNotFound,
    /// A compile error (Rust/TS/etc.).
    CompileError,
    /// A test failure.
    TestFailure,
    /// An operation timed out.
    Timeout,
    /// A non-MCP tool name that does not exist (wrong/deferred tool).
    ToolNotAvailable,
    /// A tool was called with invalid arguments.
    InputValidation,
    /// A fetched URL answered with an HTTP error (404, 500, ...).
    WebError,
    /// A file the reader refuses as binary — usually an encoding mismatch,
    /// not a missing file.
    BinaryFile,
    /// An MCP server returned an error (auth, validation, upstream).
    McpError,
    /// A user-stopped or cascade-cancelled call — not a code failure.
    Cancelled,
    /// A transient infrastructure error (model unavailable, etc.).
    Transient,
    /// A generic command failure (non-zero exit) with no more specific cause.
    CommandFailed,
    /// Anything else.
    Other,
}

impl ErrorCategory {
    pub fn label(self) -> &'static str {
        match self {
            ErrorCategory::EditPrecondition => "edit-precondition",
            ErrorCategory::BlockedByHook => "blocked-by-hook",
            ErrorCategory::CommandNotFound => "command-not-found",
            ErrorCategory::PathNotFound => "path-not-found",
            ErrorCategory::CompileError => "compile-error",
            ErrorCategory::TestFailure => "test-failure",
            ErrorCategory::Timeout => "timeout",
            ErrorCategory::ToolNotAvailable => "tool-not-available",
            ErrorCategory::InputValidation => "input-validation",
            ErrorCategory::WebError => "web-error",
            ErrorCategory::BinaryFile => "binary-file",
            ErrorCategory::McpError => "mcp-error",
            ErrorCategory::Cancelled => "cancelled",
            ErrorCategory::Transient => "transient",
            ErrorCategory::CommandFailed => "command-failed",
            ErrorCategory::Other => "other",
        }
    }

    /// Parse a stored category label back into the enum.
    pub fn from_label(label: &str) -> ErrorCategory {
        match label {
            "edit-precondition" => ErrorCategory::EditPrecondition,
            "blocked-by-hook" => ErrorCategory::BlockedByHook,
            "command-not-found" => ErrorCategory::CommandNotFound,
            "path-not-found" => ErrorCategory::PathNotFound,
            "compile-error" => ErrorCategory::CompileError,
            "test-failure" => ErrorCategory::TestFailure,
            "timeout" => ErrorCategory::Timeout,
            "tool-not-available" => ErrorCategory::ToolNotAvailable,
            "input-validation" => ErrorCategory::InputValidation,
            "web-error" => ErrorCategory::WebError,
            "binary-file" => ErrorCategory::BinaryFile,
            "mcp-error" => ErrorCategory::McpError,
            "cancelled" => ErrorCategory::Cancelled,
            "transient" => ErrorCategory::Transient,
            "command-failed" => ErrorCategory::CommandFailed,
            _ => ErrorCategory::Other,
        }
    }

    /// Whether recurring instances point to something the user can fix. The
    /// non-actionable rest is noise: user stops (`Cancelled`), infra blips
    /// (`Transient`), and the unclassified buckets (`CommandFailed`, `Other`)
    /// whose cause the heuristics could not pin down. Reports filter on this so
    /// "top friction" stays a list of leverage, not a tally of everything.
    pub fn is_actionable(self) -> bool {
        !matches!(
            self,
            ErrorCategory::Cancelled
                | ErrorCategory::Transient
                | ErrorCategory::CommandFailed
                | ErrorCategory::Other
        )
    }

    /// What recurring instances of this category suggest fixing. The
    /// instruction file varies by harness (`CLAUDE.md` vs `AGENTS.md`), so it
    /// arrives as a parameter rather than living in the shared vocabulary.
    pub fn suggestion(self, instruction_file: &str) -> String {
        match self {
            ErrorCategory::EditPrecondition => {
                "re-read before editing; a stale or fuzzy match keeps failing".to_string()
            }
            ErrorCategory::BlockedByHook => {
                "a habit keeps hitting a rule/hook — the rule blocks but doesn't prevent it"
                    .to_string()
            }
            ErrorCategory::CommandNotFound => {
                "a missing tool; install it or note its absence".to_string()
            }
            ErrorCategory::PathNotFound => {
                format!(
                    "paths are being guessed wrong — a file map in {instruction_file} would help"
                )
            }
            ErrorCategory::CompileError => {
                "expected during iteration; spikes may signal a hard spot".to_string()
            }
            ErrorCategory::TestFailure => {
                "flaky or hard tests; worth stabilising the worst".to_string()
            }
            ErrorCategory::Timeout => "slow commands; raise timeout or split the work".to_string(),
            ErrorCategory::ToolNotAvailable => {
                "a tool was called by a name that doesn't exist — wrong/deferred tool".to_string()
            }
            ErrorCategory::InputValidation => "tool called with invalid arguments".to_string(),
            ErrorCategory::WebError => {
                "a fetched URL returned an HTTP error — a dead or moved link; update or drop it"
                    .to_string()
            }
            ErrorCategory::BinaryFile => {
                "a file the reader refuses as binary — usually an encoding mismatch; convert it \
                 or stop pointing at it"
                    .to_string()
            }
            ErrorCategory::McpError => {
                "an MCP server erroring (auth/validation/upstream)".to_string()
            }
            ErrorCategory::Cancelled => {
                "not a code failure: a user stop or parallel-cascade cancel; frequent stops \
                 may mean the work went off-track"
                    .to_string()
            }
            ErrorCategory::Transient => {
                "transient infra (e.g. model unavailable) — not yours to fix".to_string()
            }
            ErrorCategory::CommandFailed => {
                "a command exited non-zero; cause unclassified".to_string()
            }
            ErrorCategory::Other => "uncategorised — inspect the raw errors".to_string(),
        }
    }
}

/// Classify an error tool-result's text. Order matters: more specific patterns
/// (which may also contain generic phrases like "not found") are checked first.
///
/// This is the **fallback**: a denied call is classified from the structural
/// marker the transcript carries instead (`adapter::transcript`), because denial
/// text is hook-authored and may be in any language. Text matching only has to
/// cover what carries no such marker.
///
/// Editing these patterns changes what an unchanged transcript yields, so it
/// requires bumping `CLAUDE_ANALYZER_VERSION` (`adapter::transcript`) — otherwise already-ingested
/// sessions keep their old categories forever (`docs/specs/storage.md`).
pub fn classify_error(text: &str) -> ErrorCategory {
    let lower = text.to_lowercase();
    let has = |needle: &str| lower.contains(needle);

    if has("string to replace not found")
        || has("matches of the string to replace")
        || has("old_string and new_string are exactly the same")
        || has("has not been read")
        || has("has been modified")
        || has("file has not been read yet")
        || has("could not find oldstring")
    {
        ErrorCategory::EditPrecondition
    } else if has("cancelled: parallel tool call")
        || has("request interrupted by user")
        || has("the user doesn't want to proceed")
        || has("tool execution aborted")
        || has("user rejected permission")
        || has("task cancelled")
    {
        // User stop or a sibling failing in a parallel batch — not a code fault.
        ErrorCategory::Cancelled
    } else if has("permission for this action was denied")
        || has("enforce-perl")
        || has("use perl instead")
        || has("denied by")
        || has("blocked:")
        || has("prevents you from using")
    {
        ErrorCategory::BlockedByHook
    } else if has("temporarily unavailable") || has("overloaded") {
        ErrorCategory::Transient
    } else if has("non 2xx status code") {
        // A fetched URL answered with an HTTP error — checked before the file
        // patterns since the URL may contain any of their phrases.
        ErrorCategory::WebError
    } else if has("cannot read binary file") {
        // The reader refuses the file's encoding, not its existence — checked
        // before the path patterns for the same reason.
        ErrorCategory::BinaryFile
    } else if has("no such tool available")
        || has("not found. available skills")
        // kilocode answers a wrong tool name with "Unknown tool "edit". This
        // tool does not exist." — the tool name is the missing thing, not a
        // file, so this is checked before the path vocabulary below.
        || has("unknown tool")
    {
        ErrorCategory::ToolNotAvailable
    } else if has("inputvalidationerror")
        || has("input validation error")
        || has("called with invalid arguments")
    {
        ErrorCategory::InputValidation
    } else if has("mcp error") {
        ErrorCategory::McpError
    } else if has("command not found") || has("is not recognized") {
        ErrorCategory::CommandNotFound
    } else if has("no such file")
        || has("does not exist")
        || has("cannot find the path")
        || has("file not found")
    {
        ErrorCategory::PathNotFound
    } else if has("error[e") || has("mismatched types") || has("cannot borrow") {
        ErrorCategory::CompileError
    } else if has("test result: failed") || has("assertion `left") || has("panicked at") {
        ErrorCategory::TestFailure
    } else if has("timed out") || has("timeout") {
        ErrorCategory::Timeout
    } else if has("exit code") {
        // Generic non-zero exit, checked last so specific causes above win.
        ErrorCategory::CommandFailed
    } else {
        ErrorCategory::Other
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_unknown_tool_name_is_not_a_missing_path() {
        // kilocode answers a wrong tool name with "Unknown tool "edit". This
        // tool does not exist." — the "does not exist" in that message matched
        // the path vocabulary and mis-classified the failure; the tool name is
        // the missing thing, not a file.
        assert_eq!(
            classify_error(
                "Unknown tool \"edit\". This tool does not exist. Please use one of the available tools: execute_command, read_file, write_to_file"
            ),
            ErrorCategory::ToolNotAvailable
        );
    }

    #[test]
    fn binary_file_reads_are_their_own_category() {
        assert_eq!(
            classify_error("Cannot read binary file: /tmp/example/notes/2026-02-15.md"),
            ErrorCategory::BinaryFile
        );
    }

    #[test]
    fn http_status_errors_are_their_own_category() {
        assert_eq!(
            classify_error(
                "StatusCode: non 2xx status code (404 GET https://example.com/main/README.md)"
            ),
            ErrorCategory::WebError
        );
    }

    #[test]
    fn edit_precondition_is_detected_first() {
        assert_eq!(
            classify_error("String to replace not found in file"),
            ErrorCategory::EditPrecondition
        );
        assert_eq!(
            classify_error("File has been modified since read, either by the user"),
            ErrorCategory::EditPrecondition
        );
    }

    #[test]
    fn edit_preconditions_beyond_a_missing_string() {
        // Both texts are emitted verbatim by the Edit tool, so matching them is
        // precise rather than a guess at free-form wording.
        assert_eq!(
            classify_error("Found 2 matches of the string to replace, but replace_all is false"),
            ErrorCategory::EditPrecondition
        );
        assert_eq!(
            classify_error("No changes to make: old_string and new_string are exactly the same."),
            ErrorCategory::EditPrecondition
        );
    }

    #[test]
    fn hook_and_permission_blocks() {
        assert_eq!(
            classify_error("Permission for this action was denied by the user"),
            ErrorCategory::BlockedByHook
        );
        assert_eq!(
            classify_error("[enforce-perl.sh]: sed/awk detected - Use perl instead"),
            ErrorCategory::BlockedByHook
        );
    }

    #[test]
    fn command_not_found_beats_generic_not_found() {
        assert_eq!(
            classify_error("bash: foo: command not found"),
            ErrorCategory::CommandNotFound
        );
    }

    #[test]
    fn path_not_found() {
        assert_eq!(
            classify_error("cat: /tmp/x: No such file or directory"),
            ErrorCategory::PathNotFound
        );
    }

    #[test]
    fn opencode_phrasings_reach_their_categories() {
        // Observed opencode tool-error wordings; each is specific enough to
        // carry no other meaning in either harness's error texts.
        assert_eq!(
            classify_error("File not found: /tmp/example/foo.md"),
            ErrorCategory::PathNotFound
        );
        assert_eq!(
            classify_error("Could not find oldString in the file. It must match exactly,"),
            ErrorCategory::EditPrecondition
        );
        assert_eq!(
            classify_error("Tool execution aborted"),
            ErrorCategory::Cancelled
        );
        assert_eq!(
            classify_error("The user rejected permission to use this specific tool call."),
            ErrorCategory::Cancelled
        );
        assert_eq!(classify_error("Task cancelled"), ErrorCategory::Cancelled);
        assert_eq!(
            classify_error("The user has specified a rule which prevents you from using bash."),
            ErrorCategory::BlockedByHook
        );
        assert_eq!(
            classify_error("The edit tool was called with invalid arguments"),
            ErrorCategory::InputValidation
        );
        assert_eq!(
            classify_error("Skill \"nikki-make\" not found. Available skills: ..."),
            ErrorCategory::ToolNotAvailable
        );
    }

    #[test]
    fn the_path_suggestion_names_the_harness_instruction_file() {
        assert!(
            ErrorCategory::PathNotFound
                .suggestion("CLAUDE.md")
                .contains("a file map in CLAUDE.md would help")
        );
        assert!(
            ErrorCategory::PathNotFound
                .suggestion("AGENTS.md")
                .contains("a file map in AGENTS.md would help")
        );
    }

    #[test]
    fn compile_and_test_and_timeout() {
        assert_eq!(
            classify_error("error[E0599]: no method named foo"),
            ErrorCategory::CompileError
        );
        assert_eq!(
            classify_error("test result: FAILED. 1 failed"),
            ErrorCategory::TestFailure
        );
        assert_eq!(
            classify_error("Command timed out after 120s"),
            ErrorCategory::Timeout
        );
    }

    #[test]
    fn user_stops_and_cascades_are_cancelled_not_failures() {
        assert_eq!(
            classify_error("<tool_use_error>Cancelled: parallel tool call Bash(grep ...)"),
            ErrorCategory::Cancelled
        );
        assert_eq!(
            classify_error("The user doesn't want to proceed with this tool use."),
            ErrorCategory::Cancelled
        );
    }

    #[test]
    fn infra_tool_and_validation_errors() {
        assert_eq!(
            classify_error("claude-opus-4-8 is temporarily unavailable"),
            ErrorCategory::Transient
        );
        assert_eq!(
            classify_error("<tool_use_error>Error: No such tool available: Glob"),
            ErrorCategory::ToolNotAvailable
        );
        assert_eq!(
            classify_error("InputValidationError: Read failed"),
            ErrorCategory::InputValidation
        );
        assert_eq!(
            classify_error("MCP error -32602: Input validation"),
            ErrorCategory::McpError
        );
    }

    #[test]
    fn a_specific_cause_beats_generic_exit_code() {
        // A compile error that also prints "Exit code 1" is a compile error.
        assert_eq!(
            classify_error("error[E0599]: no method\nExit code 1"),
            ErrorCategory::CompileError
        );
        // A bare non-zero exit with no known cause is command-failed.
        assert_eq!(
            classify_error("Exit code 2 (eval): bad substitution"),
            ErrorCategory::CommandFailed
        );
    }

    #[test]
    fn unrecognised_is_other() {
        assert_eq!(
            classify_error("something weird happened"),
            ErrorCategory::Other
        );
    }

    #[test]
    fn noise_categories_are_not_actionable() {
        // Fixable friction the user can act on.
        assert!(ErrorCategory::EditPrecondition.is_actionable());
        assert!(ErrorCategory::PathNotFound.is_actionable());
        // Noise: stops, infra, and the unclassified buckets.
        assert!(!ErrorCategory::Cancelled.is_actionable());
        assert!(!ErrorCategory::Transient.is_actionable());
        assert!(!ErrorCategory::CommandFailed.is_actionable());
        assert!(!ErrorCategory::Other.is_actionable());
    }
}
