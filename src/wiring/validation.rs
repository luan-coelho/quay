//! Form validation schemas backed by the `validator` crate.
//!
//! Each user-facing form has a matching `Validate` struct. The
//! `on_submit_*` callbacks in sibling modules build one of these,
//! call `.validate()`, and fan out field errors into Slint
//! properties so the UI renders inline messages under the relevant
//! Input — matching the shadcn/react-hook-form pattern of per-field
//! error text in destructive red below the control.
//!
//! The validation rules encode the same checks that previously lived
//! inline in `task_callbacks.rs` / `project_callbacks.rs` and were
//! surfaced as transient toasts (`show_toast("error", ...)`). Moving
//! them here gives us:
//!
//! 1. A single source of truth per form, declaratively typed.
//! 2. Easy unit testability (no Slint or AppState needed).
//! 3. Structured field errors ready to wire into Slint `error`
//!    properties via [`first_errors`].

use std::borrow::Cow;
use std::collections::HashMap;
use std::path::Path;

use validator::{Validate, ValidationError};

/// Form backing the "New Task" modal (Cmd+N). Retained for test coverage.
#[derive(Debug, Validate)]
#[allow(dead_code)]
pub struct NewTaskForm {
    #[validate(length(min = 1, message = "Title is required"))]
    pub title: String,
}

/// Form backing the "New Project" modal (sidebar + button).
#[derive(Debug, Validate)]
pub struct NewProjectForm {
    #[validate(length(min = 1, message = "Name is required"))]
    pub name: String,

    #[validate(
        length(min = 1, message = "Repository path is required"),
        custom(function = "validate_absolute_path")
    )]
    pub repo_path: String,
}

/// Form backing the live-edited task title in the Description panel.
/// There's no submit button — this is validated on every change via
/// `on_title_changed` so the user sees the error the instant they
/// empty the field.
#[derive(Debug, Validate)]
pub struct TaskTitleForm {
    #[validate(length(min = 1, message = "Title is required"))]
    pub title: String,
}

/// Custom validator: repository path must be absolute and exist on
/// the filesystem. Both checks are synchronous — they only hit
/// `std::path::Path` APIs and a `std::fs::metadata` under the hood,
/// which is fine for a one-shot validation on submit.
fn validate_absolute_path(path: &str) -> Result<(), ValidationError> {
    let p = Path::new(path);
    if !p.is_absolute() {
        return Err(ValidationError::new("not_absolute")
            .with_message(Cow::from("Path must be absolute")));
    }
    if !p.exists() {
        return Err(ValidationError::new("not_found")
            .with_message(Cow::from("Path does not exist")));
    }
    Ok(())
}

/// Map a [`ValidationErrors`](validator::ValidationErrors) bag to a
/// plain `HashMap<field_name, first_error_message>` — the shape the
/// Slint wiring layer actually needs. Only the first error per field
/// is kept (shadcn convention: show one message at a time).
///
/// Field names are the struct field names (e.g. "title", "repo_path").
pub fn first_errors(
    errs: &validator::ValidationErrors,
) -> HashMap<String, String> {
    let mut out = HashMap::new();
    for (field, field_errors) in errs.field_errors() {
        let msg = field_errors
            .first()
            .and_then(|e| e.message.as_ref().map(|m| m.to_string()))
            .unwrap_or_else(|| "Invalid".to_string());
        out.insert(field.to_string(), msg);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_task_empty_title_fails() {
        let form = NewTaskForm { title: String::new() };
        let errs = form.validate().expect_err("empty title must fail");
        let map = first_errors(&errs);
        assert_eq!(map.get("title").map(String::as_str), Some("Title is required"));
    }

    #[test]
    fn new_task_nonempty_title_passes() {
        let form = NewTaskForm { title: "Add dark mode".into() };
        assert!(form.validate().is_ok());
    }

    #[test]
    fn new_project_empty_name_fails() {
        let form = NewProjectForm {
            name: String::new(),
            repo_path: std::env::temp_dir().to_string_lossy().into_owned(),
        };
        let errs = form.validate().expect_err("empty name must fail");
        let map = first_errors(&errs);
        assert_eq!(map.get("name").map(String::as_str), Some("Name is required"));
    }

    #[test]
    fn new_project_relative_path_fails() {
        let form = NewProjectForm {
            name: "demo".into(),
            repo_path: "relative/path".into(),
        };
        let errs = form.validate().expect_err("relative path must fail");
        let map = first_errors(&errs);
        assert_eq!(
            map.get("repo_path").map(String::as_str),
            Some("Path must be absolute"),
        );
    }

    #[test]
    fn new_project_nonexistent_absolute_path_fails() {
        // Build a nonexistent absolute path that is absolute on every
        // platform (Unix uses `/…`, Windows needs a drive prefix).
        let nonexistent = std::env::temp_dir()
            .join("nonexistent_quay_test_dir_abc123xyz");
        let form = NewProjectForm {
            name: "demo".into(),
            repo_path: nonexistent.to_string_lossy().into_owned(),
        };
        let errs = form.validate().expect_err("nonexistent path must fail");
        let map = first_errors(&errs);
        assert_eq!(
            map.get("repo_path").map(String::as_str),
            Some("Path does not exist"),
        );
    }

    #[test]
    fn new_project_valid_passes() {
        // temp_dir() is absolute and exists on every platform.
        let form = NewProjectForm {
            name: "demo".into(),
            repo_path: std::env::temp_dir().to_string_lossy().into_owned(),
        };
        assert!(form.validate().is_ok());
    }

    #[test]
    fn task_title_empty_fails() {
        let form = TaskTitleForm { title: String::new() };
        assert!(form.validate().is_err());
    }
}
