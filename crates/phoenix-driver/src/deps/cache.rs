//! Locating the dependency cache.
//!
//! Git dependencies are fetched into `$PHOENIX_HOME/cache` (default
//! `~/.phoenix/cache`), never inside the project tree (a locked Phase 3.1
//! decision — see `docs/design-decisions.md` §Phase 3.1). The cache is keyed by
//! URL and resolved commit SHA so distinct revisions of the same repo coexist
//! and a clean checkout rebuilds reproducibly from the lockfile.
//!
//! Layout (mirrors the shape Cargo uses for git sources):
//!
//! ```text
//! $PHOENIX_HOME/cache/git/db/<slug>/            # one fetched clone per URL
//! $PHOENIX_HOME/cache/git/checkouts/<slug>/<sha>/   # a worktree per commit
//! ```
//!
//! `<slug>` is a sanitized, hashed form of the URL so two URLs never collide
//! and the directory name stays filesystem-safe.

use std::ffi::OsString;
use std::hash::{Hash, Hasher};
use std::path::PathBuf;

/// The Phoenix home directory: `$PHOENIX_HOME` if set to a non-empty value, else
/// `~/.phoenix`. See [`phoenix_home_from`] for the exact resolution policy and
/// its rationale; this wrapper only supplies the live environment.
pub fn phoenix_home() -> Option<PathBuf> {
    #[allow(deprecated)]
    // `std::env::home_dir`'s surprising Windows `$HOME` behavior was corrected in
    // Rust 1.85 (the edition-2024 floor this workspace builds on) and the function
    // was un-deprecated in 1.87, making it the dependency-free way to find the home
    // directory again. The `allow(deprecated)` keeps it warning-free on the 1.85/
    // 1.86 floor, where the un-deprecation hasn't landed yet; it is a harmless
    // no-op on 1.87+.
    let home = std::env::home_dir();
    phoenix_home_from(std::env::var_os("PHOENIX_HOME"), home)
}

/// Pure resolution policy behind [`phoenix_home`], split out so the env-override
/// / home-fallback rules can be unit-tested without mutating the process-global
/// environment (`set_var` is racy under the parallel test runner).
///
/// An empty `$PHOENIX_HOME` is treated as unset rather than yielding a relative
/// cache root (which `PathBuf::from("")` would, placing the cache under the
/// current working directory — contrary to the "never inside the project tree"
/// rule). A *non-empty* override, by contrast, is authoritative and honored
/// verbatim — including a relative path: an explicit `$PHOENIX_HOME=cache` is the
/// caller's deliberate choice, so we don't second-guess it the way we do the
/// empty value (which is indistinguishable from unset). The "never inside the
/// project tree" rule governs the *default* location, not an explicit opt-in.
/// Returns `None` only when `$PHOENIX_HOME` is unset/empty *and* a home directory
/// cannot be determined — callers surface that as an actionable error rather than
/// guessing a location inside the project tree.
fn phoenix_home_from(env_override: Option<OsString>, home: Option<PathBuf>) -> Option<PathBuf> {
    if let Some(dir) = env_override.filter(|v| !v.is_empty()) {
        return Some(PathBuf::from(dir));
    }
    home.map(|h| h.join(".phoenix"))
}

/// The default dependency cache directory (`<phoenix_home>/cache`), or `None`
/// if [`phoenix_home`] could not be determined.
pub fn default_cache_dir() -> Option<PathBuf> {
    phoenix_home().map(|h| h.join("cache"))
}

/// A filesystem-safe, collision-resistant directory name for a git URL: the
/// repo's last path segment (sans `.git`) plus a hash of the full URL.
pub fn url_slug(url: &str) -> String {
    let tail = url
        .rsplit(['/', ':'])
        .find(|s| !s.is_empty())
        .unwrap_or("repo");
    let tail = tail.strip_suffix(".git").unwrap_or(tail);
    let sanitized: String = tail
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '-'
            }
        })
        .collect();
    let sanitized = if sanitized.is_empty() {
        "repo".to_string()
    } else {
        sanitized
    };
    // `DefaultHasher::new()` is seeded deterministically, so the slug is stable
    // within a toolchain — all this needs, since the slug is only a cache-locality
    // key, never a stability guarantee. Its output may differ across Rust
    // versions; that is harmless here, because a changed slug merely triggers a
    // re-clone. Reproducibility comes from the lockfile's pinned SHA, not the slug.
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    url.hash(&mut hasher);
    format!("{sanitized}-{:016x}", hasher.finish())
}

/// The bare-ish fetched-clone directory for a URL under `cache_root`.
pub fn git_db_dir(cache_root: &std::path::Path, url: &str) -> PathBuf {
    cache_root.join("git").join("db").join(url_slug(url))
}

/// The per-commit checkout directory for a URL + SHA under `cache_root`.
pub fn git_checkout_dir(cache_root: &std::path::Path, url: &str, sha: &str) -> PathBuf {
    cache_root
        .join("git")
        .join("checkouts")
        .join(url_slug(url))
        .join(sha)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn phoenix_home_prefers_nonempty_env_override() {
        let got = phoenix_home_from(
            Some(OsString::from("/custom/ph")),
            Some(PathBuf::from("/home/u")),
        );
        assert_eq!(got, Some(PathBuf::from("/custom/ph")));
    }

    #[test]
    fn phoenix_home_treats_empty_env_as_unset() {
        // An empty $PHOENIX_HOME must fall back to ~/.phoenix, never collapse to a
        // relative cache root under the cwd (the "never inside the project tree"
        // rule).
        let got = phoenix_home_from(Some(OsString::new()), Some(PathBuf::from("/home/u")));
        assert_eq!(got, Some(PathBuf::from("/home/u/.phoenix")));
    }

    #[test]
    fn phoenix_home_falls_back_to_home_when_env_unset() {
        let got = phoenix_home_from(None, Some(PathBuf::from("/home/u")));
        assert_eq!(got, Some(PathBuf::from("/home/u/.phoenix")));
    }

    #[test]
    fn phoenix_home_is_none_without_env_or_home() {
        // Neither an override nor a discoverable home dir: callers must surface an
        // actionable error rather than guess a path inside the project tree. Both
        // the unset and empty-override cases collapse to `None` here.
        assert_eq!(phoenix_home_from(None, None), None);
        assert_eq!(phoenix_home_from(Some(OsString::new()), None), None);
    }

    #[test]
    fn slug_is_stable_and_sanitized() {
        let a = url_slug("https://github.com/example/http.git");
        let b = url_slug("https://github.com/example/http.git");
        assert_eq!(a, b, "slug must be deterministic");
        assert!(
            a.starts_with("http-"),
            "slug should carry the repo name: {a}"
        );
        assert!(
            a.chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'),
            "slug must be filesystem-safe: {a}"
        );
    }

    #[test]
    fn distinct_urls_get_distinct_slugs() {
        assert_ne!(
            url_slug("https://github.com/a/http.git"),
            url_slug("https://github.com/b/http.git")
        );
    }

    #[test]
    fn checkout_dir_separates_by_sha() {
        let root = std::path::Path::new("/cache");
        let a = git_checkout_dir(root, "u/http.git", "aaaa");
        let b = git_checkout_dir(root, "u/http.git", "bbbb");
        assert_ne!(a, b);
        assert!(a.ends_with("aaaa"));
    }
}
