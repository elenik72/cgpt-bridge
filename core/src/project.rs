//! The ChatGPT project a new chat of the bridge goes into: the one named
//! after the project folder. Shared by the CLI (the repository root above
//! its current directory, see [`project_root`]) and the gateway (OpenCode's
//! workspace root).

use std::path::{Path, PathBuf};

use cgpt_bridge_protocol::check_project_name;

use crate::redact::redact;

/// The project folder the CLI works in: the nearest folder from `start` up
/// that holds `.git` (a directory, or the file of a worktree or submodule:
/// a worktree is named after its own folder); `start` itself when there is
/// none. The walk stops at the home folder `home` (a dotfiles repository in
/// the home folder is no project root). Each folder of the walk is compared
/// with `home` as [`same_folder`] does, so a symlink, a Windows 8.3 short
/// name (`RUNNER~1`) or a resolved `\\?\C:\…` path that reaches the home
/// folder stops it. The result keeps `start`'s form.
pub fn project_root(start: &Path, home: Option<&Path>) -> PathBuf {
    let home = home.map(resolved_key);
    for dir in start.ancestors() {
        if home.as_ref().is_some_and(|h| resolved_key(dir) == *h) {
            break;
        }
        if dir.join(".git").exists() {
            return dir.to_path_buf();
        }
    }
    start.to_path_buf()
}

/// The project name for folder `dir`: its last component. `Ok(None)` for a
/// disk root (`/`, `C:\`) and for the home folder `home`: no project there.
/// `Err` with the reason for a name that cannot be sent (see
/// [`check_project_name`]) or that the redaction rules would change (the
/// name goes to ChatGPT like any other text); the caller says so, the new
/// chat is a plain one. Both `/` and `\` separate components, so a Windows
/// path from OpenCode reads the same on any system.
pub fn project_name(dir: &str, home: Option<&str>) -> Result<Option<String>, String> {
    let dir = plain_dir(dir.trim());
    let dir = trim_dir(&dir);
    if dir.is_empty() || is_drive(dir) {
        return Ok(None);
    }
    if home.is_some_and(|h| same_folder(Path::new(dir), Path::new(h))) {
        return Ok(None);
    }
    let Some(name) = dir.rsplit(['/', '\\']).next().map(str::trim) else {
        return Ok(None);
    };
    check_project_name(name)?;
    if redact(name, "").stdout != name {
        return Err("the redaction rules change the project name".to_string());
    }
    Ok(Some(name.to_string()))
}

/// The warning for a folder whose name cannot be a project name: the same
/// outcome as a project the page could not open (a plain new chat).
pub fn refused_warning(reason: &str) -> String {
    format!(
        "the folder name cannot be a ChatGPT project name ({}); a new chat goes to a plain new chat.",
        reason
    )
}

/// `dir` without trailing separators and surrounding whitespace.
fn trim_dir(dir: &str) -> &str {
    dir.trim().trim_end_matches(['/', '\\'])
}

/// `C:` (what is left of `C:\`).
fn is_drive(dir: &str) -> bool {
    let b = dir.as_bytes();
    b.len() == 2 && b[0].is_ascii_alphabetic() && b[1] == b':'
}

/// `dir` without the verbatim prefix of a resolved Windows path.
fn plain_dir(dir: &str) -> String {
    if let Some(rest) = dir.strip_prefix(r"\\?\UNC\") {
        format!(r"\\{rest}")
    } else {
        dir.strip_prefix(r"\\?\").unwrap_or(dir).to_string()
    }
}

/// Whether two folders are the same: both resolved where they exist (links,
/// Windows 8.3 short names), as they are written otherwise, and compared as
/// [`dir_key`] reads them.
pub fn same_folder(a: &Path, b: &Path) -> bool {
    resolved_key(a) == resolved_key(b)
}

// `dir_key` of `dir` resolved (`canonicalize`), or as written when it cannot
// be (it does not exist here, say a path from OpenCode on another system).
fn resolved_key(dir: &Path) -> String {
    let resolved = std::fs::canonicalize(dir).unwrap_or_else(|_| dir.to_path_buf());
    dir_key(&resolved.to_string_lossy())
}

/// A folder as compared with another: without the verbatim prefix Windows
/// gives a resolved path (`\\?\C:\…`, `\\?\UNC\server\…`), with `/` for
/// every separator and no trailing one, and lowercased when it is a Windows
/// path (one with a drive or a `\`), whose names have no case.
pub fn dir_key(dir: &str) -> String {
    let dir = dir.trim();
    let windows = dir.contains('\\') || dir.as_bytes().get(1) == Some(&b':');
    let key = plain_dir(dir).replace('\\', "/");
    let key = key.trim_end_matches('/');
    if windows {
        key.to_lowercase()
    } else {
        key.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn name(dir: &str, home: Option<&str>) -> Option<String> {
        project_name(dir, home).unwrap()
    }

    #[test]
    fn the_last_folder_names_the_project() {
        assert_eq!(
            name("/home/u/dev/cgpt-bridge", None).as_deref(),
            Some("cgpt-bridge")
        );
        assert_eq!(
            name("/home/u/dev/cgpt-bridge/", None).as_deref(),
            Some("cgpt-bridge")
        );
        assert_eq!(name("/srv/мой проект", None).as_deref(), Some("мой проект"));
        assert_eq!(
            name(r"C:\Users\u\src\gateway\", Some(r"C:\Users\u")).as_deref(),
            Some("gateway")
        );
    }

    #[test]
    fn no_project_for_a_root_or_the_home_folder() {
        for dir in ["/", "", "  ", r"C:\", "C:", "c:/"] {
            assert_eq!(project_name(dir, None), Ok(None), "{dir:?}");
        }
        assert_eq!(name("/home/u", Some("/home/u")), None);
        assert_eq!(name("/home/u/", Some("/home/u")), None);
        assert_eq!(name(r"C:\Users\U", Some(r"c:\users\u\")), None);
        // Only the home folder itself: one below it has a name.
        assert_eq!(name("/home/u/app", Some("/home/u")).as_deref(), Some("app"));
        // A Unix path is compared as written.
        assert_eq!(name("/home/U", Some("/home/u")).as_deref(), Some("U"));
    }

    #[test]
    fn a_name_that_cannot_be_sent_is_refused_with_the_reason() {
        let long = format!("/x/{}", "a".repeat(101));
        assert!(project_name(&long, None)
            .unwrap_err()
            .contains("101 characters"));
        assert!(project_name("/x/a\u{1b}b", None)
            .unwrap_err()
            .contains("control"));
        let max = format!("/x/{}", "a".repeat(100));
        assert_eq!(name(&max, None).map(|n| n.len()), Some(100));
        assert!(refused_warning("why").contains("(why); a new chat goes to a plain new chat"));
    }

    #[test]
    fn a_name_the_redaction_rules_change_is_refused() {
        let key = format!("sk-proj-{}", "Ab3dE5gH7jK9mN1pQ3sT5vW7yZ9bC1dF");
        assert_ne!(
            redact(&key, "").stdout,
            key,
            "the test needs a name that is redacted"
        );
        assert_eq!(
            project_name(&format!("/x/{key}"), None),
            Err("the redaction rules change the project name".to_string())
        );
    }

    #[test]
    fn a_resolved_windows_home_matches_the_plain_one_without_case() {
        assert_eq!(dir_key(r"\\?\C:\Users\X\"), "c:/users/x");
        assert_eq!(dir_key(r"c:\users\x"), "c:/users/x");
        assert_eq!(dir_key(r"\\?\UNC\srv\home\x"), "//srv/home/x");
        assert_eq!(dir_key(r"\\srv\home\x"), "//srv/home/x");
        assert_eq!(dir_key("/home/X/"), "/home/X", "a Unix path keeps its case");
        assert_eq!(dir_key(r"C:\Users\Иван"), "c:/users/иван");
        // The home folder is no project, whichever form names it.
        assert_eq!(
            project_name(r"C:\Users\X", Some(r"\\?\c:\users\x")),
            Ok(None)
        );
        assert_eq!(
            project_name(r"\\?\C:\Users\X\", Some(r"C:\Users\x")),
            Ok(None)
        );
        assert_eq!(
            project_name(r"\\?\C:\", None),
            Ok(None),
            "a resolved drive root"
        );
        assert_eq!(
            project_name(r"C:\Users\X\app", Some(r"\\?\C:\Users\X"))
                .unwrap()
                .as_deref(),
            Some("app")
        );
    }

    /// A fresh `<tmp>/cgb-root-<tag>-<pid>` tree.
    fn tree(tag: &str) -> PathBuf {
        let base = std::env::temp_dir().join(format!("cgb-root-{}-{}", tag, std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        base
    }

    #[test]
    fn the_root_is_the_nearest_git_folder_below_home() {
        let home = tree("git");
        let repo = home.join("dev/app");
        let sub = repo.join("src/inner");
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        assert_eq!(project_root(&sub, Some(&home)), repo);
        assert_eq!(project_root(&repo, Some(&home)), repo);
        // A worktree or submodule has a `.git` file.
        let wt = home.join("wt");
        std::fs::create_dir_all(wt.join("pkg")).unwrap();
        std::fs::write(wt.join(".git"), "gitdir: elsewhere\n").unwrap();
        assert_eq!(project_root(&wt.join("pkg"), Some(&home)), wt);
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn without_a_git_folder_below_home_the_root_is_the_start() {
        let home = tree("nogit");
        // A dotfiles repository in the home folder is not a project root.
        std::fs::create_dir_all(home.join(".git")).unwrap();
        let dir = home.join("notes/2026");
        std::fs::create_dir_all(&dir).unwrap();
        assert_eq!(project_root(&dir, Some(&home)), dir);
        assert_eq!(project_root(&home, Some(&home)), home);
        // Outside the home folder the walk goes on up.
        let outside = tree("outside");
        std::fs::create_dir_all(outside.join(".git")).unwrap();
        let deep = outside.join("a");
        std::fs::create_dir_all(&deep).unwrap();
        assert_eq!(project_root(&deep, Some(&home)), outside);
        assert_eq!(project_root(&deep, None), outside);
        let _ = std::fs::remove_dir_all(&home);
        let _ = std::fs::remove_dir_all(&outside);
    }

    /// A start that reaches the home folder through a symlink still stops
    /// at it, and the link to the home folder names no project.
    #[cfg(unix)]
    #[test]
    fn a_symlink_into_the_home_folder_stops_the_walk() {
        let base = tree("link");
        let home = base.join("home");
        std::fs::create_dir_all(home.join(".git")).unwrap();
        std::fs::create_dir_all(home.join("notes")).unwrap();
        let link = base.join("link-to-home");
        std::os::unix::fs::symlink(&home, &link).unwrap();
        let start = link.join("notes");
        assert_eq!(project_root(&start, Some(&home)), start);
        assert_eq!(
            project_name(link.to_str().unwrap(), home.to_str()),
            Ok(None)
        );
        // A link inside the home folder to a folder elsewhere: the walk
        // still stops at the home folder it passes.
        let elsewhere = base.join("elsewhere/sub");
        std::fs::create_dir_all(&elsewhere).unwrap();
        std::os::unix::fs::symlink(base.join("elsewhere"), home.join("away")).unwrap();
        let start = home.join("away/sub");
        assert_eq!(project_root(&start, Some(&home)), start);
        let _ = std::fs::remove_dir_all(&base);
    }

    /// Windows: the home folder resolved (`\\?\C:\…`, as `canonicalize`
    /// gives it) and the current folder plain (as `current_dir` gives it).
    #[cfg(windows)]
    #[test]
    fn a_resolved_windows_home_stops_the_walk_and_names_no_project() {
        let home = tree("winhome");
        std::fs::create_dir_all(home.join(".git")).unwrap();
        let dir = home.join("notes");
        std::fs::create_dir_all(&dir).unwrap();
        let resolved = std::fs::canonicalize(&home).unwrap();
        assert!(
            resolved.to_string_lossy().starts_with(r"\\?\"),
            "{resolved:?}"
        );
        // A dotfiles `.git` in the home folder is no root.
        assert_eq!(project_root(&dir, Some(&resolved)), dir);
        // The home folder itself names no project.
        let plain = home.to_str().unwrap();
        assert_eq!(project_name(plain, resolved.to_str()), Ok(None));
        let upper = plain.to_uppercase();
        assert_eq!(
            project_root(Path::new(&upper), Some(&resolved)),
            PathBuf::from(&upper)
        );
        let _ = std::fs::remove_dir_all(&home);
    }
}
