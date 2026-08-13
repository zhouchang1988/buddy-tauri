//! Task ID validation — Rust port of `src/shared/task-id.ts` (added upstream
//! in v1.2.15). Task IDs become on-disk directory names, so `create_task`
//! validates before touching the filesystem.

/// Maximum length in Unicode code points (not UTF-16 units / bytes).
pub const TASK_ID_MAX_CODE_POINTS: usize = 64;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskIdValidationReason {
    Empty,
    TooLong,
    DotSegment,
    PathSeparator,
    ControlCharacter,
}

impl TaskIdValidationReason {
    /// The exact strings the Electron edition puts into
    /// `Invalid task ID: <reason>` errors.
    pub fn as_str(&self) -> &'static str {
        match self {
            TaskIdValidationReason::Empty => "empty",
            TaskIdValidationReason::TooLong => "too_long",
            TaskIdValidationReason::DotSegment => "dot_segment",
            TaskIdValidationReason::PathSeparator => "path_separator",
            TaskIdValidationReason::ControlCharacter => "control_character",
        }
    }
}

impl std::fmt::Display for TaskIdValidationReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskIdValidation {
    pub value: String,
    pub reason: Option<TaskIdValidationReason>,
}

// Task IDs become on-disk directory names. The policy below is intentionally
// permissive: any ordinary Unicode text is allowed (CJK, smart quotes, ASCII
// punctuation, spaces, emoji, backslash), and we reject only what would break a
// macOS/Linux filename or escape the task directory.
//
//   - empty:        nothing usable after trimming
//   - too_long:     > 64 Unicode code points (not UTF-16 units)
//   - dot_segment:  '.' / '..' would make path joins resolve outside the tasks dir
//   - path_separator: '/' is the macOS/Linux filename separator
//   - control_character: C0/C1 (incl. NUL/newline) are illegal in filenames
//
// Backslash is intentionally allowed: on macOS/Linux it is a legal filename
// character and is not a path separator there. Do not add a whitelist regex or
// Unicode normalization.
pub fn validate_task_id(input: &str) -> TaskIdValidation {
    let value = input.trim().to_string();
    let reason = if value.is_empty() {
        Some(TaskIdValidationReason::Empty)
    } else if value.chars().count() > TASK_ID_MAX_CODE_POINTS {
        Some(TaskIdValidationReason::TooLong)
    } else if value == "." || value == ".." {
        Some(TaskIdValidationReason::DotSegment)
    } else if value.contains('/') {
        Some(TaskIdValidationReason::PathSeparator)
    } else if value
        .chars()
        .any(|c| ('\u{0}'..='\u{1F}').contains(&c) || ('\u{7F}'..='\u{9F}').contains(&c))
    {
        Some(TaskIdValidationReason::ControlCharacter)
    } else {
        None
    };
    TaskIdValidation { value, reason }
}

#[cfg(test)]
mod tests {
    use super::*;
    use TaskIdValidationReason as R;

    fn reason(input: &str) -> Option<TaskIdValidationReason> {
        validate_task_id(input).reason
    }

    #[test]
    fn accepts_ordinary_unicode_punctuation_and_emoji() {
        let id = "feat: \u{201C}任务名称\u{201D} (v2) [macOS + Linux] #42 🚀";
        assert_eq!(
            validate_task_id(id),
            TaskIdValidation {
                value: id.to_string(),
                reason: None,
            }
        );
    }

    #[test]
    fn accepts_a_backslash_legal_filename_character_on_macos_linux() {
        assert_eq!(
            validate_task_id("a\\b"),
            TaskIdValidation {
                value: "a\\b".to_string(),
                reason: None,
            }
        );
    }

    #[test]
    fn rejects_unsafe_task_ids() {
        for value in ["", "   ", ".", "..", "a/b", "a\nb", "a\0b"] {
            assert!(
                reason(value).is_some(),
                "expected rejection for {value:?}"
            );
        }
    }

    #[test]
    fn reports_a_distinct_reason_for_each_unsafe_case() {
        assert_eq!(reason(""), Some(R::Empty));
        assert_eq!(reason("   "), Some(R::Empty));
        assert_eq!(reason("."), Some(R::DotSegment));
        assert_eq!(reason(".."), Some(R::DotSegment));
        assert_eq!(reason("a/b"), Some(R::PathSeparator));
        assert_eq!(reason("a\nb"), Some(R::ControlCharacter));
        assert_eq!(reason("a\0b"), Some(R::ControlCharacter));
        // DEL and C1 are control characters too.
        assert_eq!(reason("a\u{7F}b"), Some(R::ControlCharacter));
        assert_eq!(reason("a\u{85}b"), Some(R::ControlCharacter));
    }

    #[test]
    fn counts_unicode_code_points_not_utf16_units() {
        let ok = "🚀".repeat(TASK_ID_MAX_CODE_POINTS);
        let too_long = "🚀".repeat(TASK_ID_MAX_CODE_POINTS + 1);
        assert_eq!(reason(&ok), None);
        assert_eq!(reason(&too_long), Some(R::TooLong));
    }

    #[test]
    fn trims_surrounding_whitespace_before_validating() {
        assert_eq!(
            validate_task_id("  hello  "),
            TaskIdValidation {
                value: "hello".to_string(),
                reason: None,
            }
        );
        assert_eq!(
            validate_task_id("  "),
            TaskIdValidation {
                value: String::new(),
                reason: Some(R::Empty),
            }
        );
    }
}
