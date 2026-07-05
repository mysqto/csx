//! Stable repository identity resolution.
//!
//! Given a working directory, [`resolve_repo_id`] derives a stable identifier
//! for the repository, preferring (in order):
//!
//! 1. The normalized `origin` remote URL (scheme, credentials, and trailing
//!    `.git` stripped, host lowercased).
//! 2. The root-commit SHA (`git rev-list --max-parents=0 HEAD`, first line).
//! 3. The working-directory path itself, as a last resort.
//!
//! All git I/O goes through the [`GitRunner`] port, so every branch here is
//! exercised by fakes in tests. Results are cached by `cwd`.

use std::collections::HashMap;

use crate::git_shim::GitRunner;

/// Resolve a stable repository id for `cwd`, caching by `cwd`.
///
/// See the module docs for the resolution order. This function never fails: if
/// git yields nothing usable, it falls back to the `cwd` path.
pub fn resolve_repo_id(
    runner: &dyn GitRunner,
    cwd: &str,
    cache: &mut HashMap<String, String>,
) -> String {
    if let Some(cached) = cache.get(cwd) {
        return cached.clone();
    }

    let id = compute_repo_id(runner, cwd);
    cache.insert(cwd.to_string(), id.clone());
    id
}

/// Compute the repo id without touching the cache.
fn compute_repo_id(runner: &dyn GitRunner, cwd: &str) -> String {
    if let Ok(out) = runner.run(cwd, &["config", "--get", "remote.origin.url"]) {
        if let Some(normalized) = normalize_remote(&out) {
            return normalized;
        }
    }

    if let Ok(out) = runner.run(cwd, &["rev-list", "--max-parents=0", "HEAD"]) {
        if let Some(sha) = first_nonempty_line(&out) {
            return sha;
        }
    }

    cwd.to_string()
}

/// Return the first non-empty, trimmed line of `s`, if any.
fn first_nonempty_line(s: &str) -> Option<String> {
    s.lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .map(str::to_string)
}

/// Normalize a git remote URL into a stable `host/path` identity.
///
/// Handles `https://`/`http://`/`git://`/`ssh://` URLs, scp-style
/// `user@host:path` remotes, and embedded credentials. The scheme, any
/// userinfo/credentials, and a trailing `.git` are removed; the host is
/// lowercased while the path is left case-sensitive.
///
/// Returns [`None`] for empty/whitespace-only input.
pub fn normalize_remote(raw: &str) -> Option<String> {
    let raw = raw.trim();
    if raw.is_empty() {
        return None;
    }

    // Strip a leading scheme like `https://`, `ssh://`, `git://`.
    let (after_scheme, had_scheme) = match raw.split_once("://") {
        Some((_scheme, rest)) => (rest, true),
        None => (raw, false),
    };

    // For scp-style remotes (no scheme) the form is `user@host:path`. Split the
    // authority from the path on the first `:` when there is no scheme, or on
    // the first `/` when there was one.
    let (authority, path) = if had_scheme {
        match after_scheme.split_once('/') {
            Some((auth, p)) => (auth, p),
            None => (after_scheme, ""),
        }
    } else {
        match after_scheme.split_once(':') {
            Some((auth, p)) => (auth, p),
            // Not scp-style and no scheme: treat the whole thing as a path with
            // no host (e.g. a local path remote).
            None => ("", after_scheme),
        }
    };

    // Drop credentials from the authority: `user:pass@host` -> `host`.
    let host = authority.rsplit_once('@').map_or(authority, |(_, h)| h);
    // Drop any port suffix on the host for identity purposes.
    let host = host.split_once(':').map_or(host, |(h, _)| h);
    let host = host.trim().to_ascii_lowercase();

    // Clean the path: trim slashes and a trailing `.git`.
    let path = path.trim().trim_start_matches('/').trim_end_matches('/');
    let path = path.strip_suffix(".git").unwrap_or(path);

    // Each surviving arm produces a non-empty identity; only the fully-empty
    // case yields `None`.
    match (host.is_empty(), path.is_empty()) {
        (true, true) => None,
        (true, false) => Some(path.to_string()),
        (false, true) => Some(host),
        (false, false) => Some(format!("{host}/{path}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::{Error, Result};
    use std::cell::RefCell;

    /// A fake [`GitRunner`] returning canned results keyed by the joined args,
    /// and recording every invocation so cache reuse can be asserted.
    #[derive(Default)]
    struct FakeGitRunner {
        responses: HashMap<String, Result<String>>,
        calls: RefCell<Vec<String>>,
    }

    impl FakeGitRunner {
        fn key(args: &[&str]) -> String {
            args.join(" ")
        }

        fn with(mut self, args: &[&str], resp: Result<String>) -> Self {
            self.responses.insert(Self::key(args), resp);
            self
        }

        fn ok(self, args: &[&str], out: &str) -> Self {
            self.with(args, Ok(out.to_string()))
        }

        fn fail(self, args: &[&str]) -> Self {
            self.with(args, Err(Error::other("no such config")))
        }

        fn call_count(&self) -> usize {
            self.calls.borrow().len()
        }
    }

    impl GitRunner for FakeGitRunner {
        fn run(&self, _cwd: &str, args: &[&str]) -> Result<String> {
            self.calls.borrow_mut().push(Self::key(args));
            match self.responses.get(&Self::key(args)) {
                Some(Ok(s)) => Ok(s.clone()),
                Some(Err(_)) => Err(Error::other("canned failure")),
                None => Err(Error::other("unexpected git call")),
            }
        }
    }

    const REMOTE: &[&str] = &["config", "--get", "remote.origin.url"];
    const ROOT: &[&str] = &["rev-list", "--max-parents=0", "HEAD"];

    #[test]
    fn resolves_from_remote() {
        let runner =
            FakeGitRunner::default().ok(REMOTE, "https://github.com/octocat/Hello-World.git\n");
        let mut cache = HashMap::new();
        let id = resolve_repo_id(&runner, "/work/x", &mut cache);
        assert_eq!(id, "github.com/octocat/Hello-World");
        // Only the remote lookup should have happened.
        assert_eq!(runner.call_count(), 1);
    }

    #[test]
    fn falls_back_to_root_commit_when_no_remote() {
        let runner = FakeGitRunner::default()
            .fail(REMOTE)
            .ok(ROOT, "abc123def456\ndeadbeef\n");
        let mut cache = HashMap::new();
        let id = resolve_repo_id(&runner, "/work/y", &mut cache);
        assert_eq!(id, "abc123def456");
        assert_eq!(runner.call_count(), 2);
    }

    #[test]
    fn empty_remote_output_falls_through_to_root_commit() {
        // Remote command succeeds but returns only whitespace -> normalize None.
        let runner = FakeGitRunner::default()
            .ok(REMOTE, "   \n")
            .ok(ROOT, "rootsha\n");
        let mut cache = HashMap::new();
        let id = resolve_repo_id(&runner, "/work/z", &mut cache);
        assert_eq!(id, "rootsha");
    }

    #[test]
    fn empty_root_output_falls_through_to_cwd() {
        let runner = FakeGitRunner::default().fail(REMOTE).ok(ROOT, "\n  \n");
        let mut cache = HashMap::new();
        let id = resolve_repo_id(&runner, "/work/only-blank", &mut cache);
        assert_eq!(id, "/work/only-blank");
    }

    #[test]
    fn falls_back_to_cwd_when_neither_available() {
        let runner = FakeGitRunner::default().fail(REMOTE).fail(ROOT);
        let mut cache = HashMap::new();
        let id = resolve_repo_id(&runner, "/tmp/not-a-repo", &mut cache);
        assert_eq!(id, "/tmp/not-a-repo");
    }

    #[test]
    fn cache_is_reused_across_calls() {
        let runner = FakeGitRunner::default().ok(REMOTE, "git@github.com:octocat/spoon.git");
        let mut cache = HashMap::new();
        let first = resolve_repo_id(&runner, "/repo", &mut cache);
        assert_eq!(first, "github.com/octocat/spoon");
        assert_eq!(runner.call_count(), 1);

        // Second call for the same cwd must hit the cache, not the runner.
        let second = resolve_repo_id(&runner, "/repo", &mut cache);
        assert_eq!(second, first);
        assert_eq!(runner.call_count(), 1);
    }

    #[test]
    fn distinct_cwds_are_cached_separately() {
        let runner = FakeGitRunner::default()
            .ok(REMOTE, "https://example.com/a/b.git")
            .ok(ROOT, "sha");
        let mut cache = HashMap::new();
        let a = resolve_repo_id(&runner, "/a", &mut cache);
        let b = resolve_repo_id(&runner, "/b", &mut cache);
        assert_eq!(a, "example.com/a/b");
        assert_eq!(b, "example.com/a/b");
        assert_eq!(cache.len(), 2);
    }

    #[test]
    fn normalize_https_strips_scheme_and_git_suffix() {
        assert_eq!(
            normalize_remote("https://github.com/Octo/Repo.git"),
            Some("github.com/Octo/Repo".into())
        );
    }

    #[test]
    fn normalize_https_without_git_suffix() {
        assert_eq!(
            normalize_remote("https://github.com/Octo/Repo"),
            Some("github.com/Octo/Repo".into())
        );
    }

    #[test]
    fn normalize_lowercases_host_only_not_path() {
        assert_eq!(
            normalize_remote("https://GitHub.COM/Octo/Repo.git"),
            Some("github.com/Octo/Repo".into())
        );
    }

    #[test]
    fn normalize_ssh_scp_form() {
        assert_eq!(
            normalize_remote("git@github.com:octo/repo.git"),
            Some("github.com/octo/repo".into())
        );
    }

    #[test]
    fn normalize_ssh_scheme_form() {
        assert_eq!(
            normalize_remote("ssh://git@github.com/octo/repo.git"),
            Some("github.com/octo/repo".into())
        );
    }

    #[test]
    fn normalize_strips_credentials() {
        assert_eq!(
            normalize_remote("https://user:token@example.com/team/proj.git"),
            Some("example.com/team/proj".into())
        );
    }

    #[test]
    fn normalize_strips_port() {
        assert_eq!(
            normalize_remote("ssh://git@example.com:2222/team/proj.git"),
            Some("example.com/team/proj".into())
        );
    }

    #[test]
    fn normalize_trims_trailing_slash() {
        assert_eq!(
            normalize_remote("https://example.com/team/proj/"),
            Some("example.com/team/proj".into())
        );
    }

    #[test]
    fn normalize_empty_is_none() {
        assert_eq!(normalize_remote(""), None);
        assert_eq!(normalize_remote("   \n  "), None);
    }

    #[test]
    fn normalize_scheme_host_only() {
        assert_eq!(
            normalize_remote("https://example.com"),
            Some("example.com".into())
        );
    }

    #[test]
    fn normalize_scheme_host_only_trailing_slash() {
        assert_eq!(
            normalize_remote("https://example.com/"),
            Some("example.com".into())
        );
    }

    #[test]
    fn normalize_local_path_remote() {
        // No scheme and no scp colon: treated as a bare path, host empty.
        assert_eq!(
            normalize_remote("/srv/git/proj.git"),
            Some("srv/git/proj".into())
        );
    }

    #[test]
    fn normalize_scp_host_only_no_path() {
        assert_eq!(
            normalize_remote("git@example.com:"),
            Some("example.com".into())
        );
    }

    #[test]
    fn normalize_scheme_only_remote_is_none() {
        // A scheme with no host and no path reaches the `(true, true)` match arm
        // and returns None.
        assert_eq!(normalize_remote("ssh://"), None);
        assert_eq!(normalize_remote("https:///"), None);
    }

    #[test]
    fn fake_git_runner_errors_on_unexpected_call() {
        // Exercise the `None` arm of the fake runner (no canned response).
        let runner = FakeGitRunner::default();
        let err = runner.run("/x", REMOTE).unwrap_err();
        assert!(err.to_string().contains("unexpected git call"));
        assert_eq!(runner.call_count(), 1);
    }
}
