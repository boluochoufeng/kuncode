//! Rendering filesystem paths as the slash-separated text the agent speaks.
//!
//! Tool arguments, tool results, and permission selectors all name files with
//! one spelling: slash-separated, valid UTF-8. Producing it by replacing `\`
//! with `/` in a rendered path is wrong on Unix, where `\` is an ordinary
//! character in a file name — the replacement quietly renames the file to one
//! that does not exist, or worse, to a different file that does. Whether a
//! separator is a separator is the platform's decision, so it is read off
//! [`Component`] instead of guessed from the text.

use std::{
    ffi::OsStr,
    path::{Component, Path},
};

/// Why a path has no faithful slash-separated form.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum PathTextError {
    /// A component is not valid UTF-8. No text form can round-trip back to the
    /// original bytes, and a lossy one would name a file nothing can open.
    NonUtf8,
    /// The path climbs above its own root.
    ParentTraversal,
}

/// Renders an absolute `path` as normalized slash-separated text.
///
/// `.` segments are dropped and `..` segments are applied lexically, so one
/// file has one spelling. Backslashes are treated as separators only inside a
/// Windows prefix, where the platform itself says they are.
///
/// The caller is expected to have established that `path` is absolute; a
/// relative path renders as though it were rooted.
///
/// # Errors
/// Returns [`PathTextError`] when a component is not UTF-8, or when `..`
/// escapes the root.
pub(crate) fn absolute_slash(path: &Path) -> Result<String, PathTextError> {
    let mut prefix = String::new();
    let mut segments = Vec::new();
    for component in path.components() {
        match component {
            // A Windows prefix (`C:`, `\\?\C:`, `\\server\share`) is the one
            // place where an embedded backslash really is structural.
            Component::Prefix(value) => prefix = utf8(value.as_os_str())?.replace('\\', "/"),
            Component::RootDir | Component::CurDir => {}
            Component::ParentDir => {
                if segments.pop().is_none() {
                    return Err(PathTextError::ParentTraversal);
                }
            }
            Component::Normal(name) => segments.push(utf8(name)?),
        }
    }
    Ok(format!("{prefix}/{}", segments.join("/")))
}

/// Renders `path` relative to `root` in the same slash-separated form, as `.`
/// when the two are the same directory.
///
/// Returns `None` when `path` cannot be written that way faithfully: it lies
/// outside `root`, or a component is not valid UTF-8.
pub(crate) fn relative_slash(root: &Path, path: &Path) -> Option<String> {
    let relative = path.strip_prefix(root).ok()?;
    let mut text = String::new();
    for component in relative.components() {
        // Stripping an absolute root leaves plain names; anything else means
        // the path was not the resolved form callers are expected to pass, and
        // is refused rather than rendered into something misleading.
        let Component::Normal(name) = component else {
            return None;
        };
        if !text.is_empty() {
            text.push('/');
        }
        text.push_str(name.to_str()?);
    }
    if text.is_empty() {
        text.push('.');
    }
    Some(text)
}

fn utf8(value: &OsStr) -> Result<&str, PathTextError> {
    value.to_str().ok_or(PathTextError::NonUtf8)
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use super::{PathTextError, absolute_slash, relative_slash};

    #[test]
    fn absolute_path_normalizes_dot_segments() {
        assert_eq!(
            absolute_slash(Path::new("/ws/./src/../src/main.rs")),
            Ok("/ws/src/main.rs".to_string())
        );
    }

    #[test]
    fn root_renders_as_a_single_slash() {
        assert_eq!(absolute_slash(Path::new("/")), Ok("/".to_string()));
    }

    #[test]
    fn parent_traversal_above_the_root_is_rejected() {
        assert_eq!(
            absolute_slash(Path::new("/ws/../..")),
            Err(PathTextError::ParentTraversal)
        );
    }

    #[test]
    fn relative_form_of_the_root_itself_is_dot() {
        assert_eq!(
            relative_slash(Path::new("/ws"), Path::new("/ws")),
            Some(".".to_string())
        );
    }

    #[test]
    fn nested_entries_are_slash_separated() {
        assert_eq!(
            relative_slash(Path::new("/ws"), Path::new("/ws/src/main.rs")),
            Some("src/main.rs".to_string())
        );
    }

    #[test]
    fn paths_outside_the_root_have_no_relative_form() {
        assert_eq!(
            relative_slash(Path::new("/ws"), Path::new("/etc/passwd")),
            None
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_backslash_in_a_unix_file_name_is_not_a_separator() {
        // The whole point: `weird\name.rs` is one file, and reporting it as
        // `weird/name.rs` would name a directory entry that does not exist —
        // or, if it does, an entirely different file.
        let name = "weird\\name.rs";
        assert_eq!(
            absolute_slash(&PathBuf::from("/ws").join(name)),
            Ok(format!("/ws/{name}"))
        );
        assert_eq!(
            relative_slash(Path::new("/ws"), &PathBuf::from("/ws").join(name)),
            Some(name.to_string())
        );
    }

    #[cfg(unix)]
    #[test]
    fn non_utf8_names_have_no_text_form() {
        use std::{ffi::OsStr, os::unix::ffi::OsStrExt};

        let name = OsStr::from_bytes(b"caf\xff");
        assert_eq!(
            absolute_slash(&PathBuf::from("/ws").join(name)),
            Err(PathTextError::NonUtf8)
        );
        assert_eq!(
            relative_slash(Path::new("/ws"), &PathBuf::from("/ws").join(name)),
            None
        );
    }
}
