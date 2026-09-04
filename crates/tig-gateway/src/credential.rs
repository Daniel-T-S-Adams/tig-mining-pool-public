//! The TIG API key (`docs/architecture.md` §2.2, slice-1 criteria H1/H3).
//!
//! §2.2 is absolute about where this may live: "The TIG API key is loaded
//! only by `tig-gateway`. It never enters the Pool API, controller, Artifact
//! Worker, member agent, database, artifact objects, logs, traces, or
//! assignment messages." Two things here enforce that rather than describe
//! it.
//!
//! [`load`] is crate-private, so no other crate can obtain a [`TigApiKey`]
//! at all — "sharing a repository or Rust library does not grant access to
//! the secret" is a compile error, not a convention.
//! `scripts/credential-boundary.sh` proves it from outside the workspace.
//!
//! And the key has no `Display`, and a `Debug` that prints nothing of it, so
//! the ordinary ways a value reaches a log or a trace do not carry it. A
//! formatting call is how a secret usually escapes, and it is not usually
//! the call anyone reviewed.

use std::path::{Path, PathBuf};

/// The TIG API key.
///
/// Deliberately opaque: no `Display`, no revealing `Debug`, no `Serialize`,
/// no `Clone` beyond what the gateway needs. The only way to see the bytes
/// is [`TigApiKey::expose`], which is crate-private and named so its use is
/// visible in review.
pub struct TigApiKey(String);

impl TigApiKey {
    /// The key material, for building an authenticated request.
    ///
    /// Crate-private and deliberately awkward to name. Every other route to
    /// the bytes — `Display`, `Debug`, serialisation — is absent.
    #[allow(dead_code, reason = "the request builder arrives with the write path")]
    pub(crate) fn expose(&self) -> &str {
        &self.0
    }

    /// Whether a key is present, without revealing anything about it.
    ///
    /// This is what §13 check 8's evidence is gathered from: the gateway
    /// reports that it holds a credential, never which.
    pub fn is_present(&self) -> bool {
        !self.0.is_empty()
    }
}

impl std::fmt::Debug for TigApiKey {
    /// Prints no part of the key, not even its length: a length leaks which
    /// of several provisioned credentials is loaded, and `{:?}` in a
    /// tracing field is exactly the accident §2.2 forbids.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("TigApiKey(<redacted>)")
    }
}

#[derive(Debug, thiserror::Error)]
pub enum CredentialError {
    #[error("no TIG API key file at {path}")]
    Missing { path: PathBuf },
    /// §2.2: "The file is readable only by the gateway identity."
    #[error(
        "{path} is readable beyond the gateway identity (mode {mode:o}); \
         architecture.md §2.2 requires it be readable only by the gateway"
    )]
    TooPermissive { path: PathBuf, mode: u32 },
    #[error("the TIG API key file at {path} is empty")]
    Empty { path: PathBuf },
    /// The error text carries the path and the reason, never the contents.
    #[error("cannot read the TIG API key file at {path}: {reason}")]
    Unreadable { path: PathBuf, reason: String },
}

/// Bytes a credential file may hold.
///
/// A key is short. A larger file is a mistake — a pasted certificate, a log,
/// the wrong path entirely — and reading it into memory to discover that
/// would be the thing §2.2 is trying to avoid.
const MAX_KEY_BYTES: u64 = 4096;

/// Load the API key from its file.
///
/// Crate-private on purpose: see the module doc. The gateway binary lives in
/// this crate; nothing else can call this.
#[allow(dead_code, reason = "the gateway binary arrives with the write path")]
pub(crate) fn load(path: &Path) -> Result<TigApiKey, CredentialError> {
    // Opened first, then checked on the OPEN HANDLE. `metadata(path)`
    // followed by `read_to_string(path)` is two lookups of the same name,
    // and a symlink or permission swap between them defeats the check §2.2
    // requires. Opening does not read; the bytes are only pulled after the
    // handle's own mode has been approved.
    let file = std::fs::File::open(path).map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            CredentialError::Missing {
                path: path.to_path_buf(),
            }
        } else {
            CredentialError::Unreadable {
                path: path.to_path_buf(),
                reason: e.to_string(),
            }
        }
    })?;
    let metadata = file.metadata().map_err(|e| CredentialError::Unreadable {
        path: path.to_path_buf(),
        reason: e.to_string(),
    })?;

    // Checked before a single byte is read. A world-readable key has already
    // failed §2.2 whether or not this process reads it, and reading it first
    // would put it in this process's memory to no purpose.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = metadata.permissions().mode() & 0o777;
        if mode & 0o077 != 0 {
            return Err(CredentialError::TooPermissive {
                path: path.to_path_buf(),
                mode,
            });
        }
    }

    if metadata.len() > MAX_KEY_BYTES {
        return Err(CredentialError::Unreadable {
            path: path.to_path_buf(),
            reason: format!(
                "{} bytes exceeds the {MAX_KEY_BYTES}-byte limit",
                metadata.len()
            ),
        });
    }

    // From the handle already approved, not by re-opening the path.
    let mut file = file;
    let mut contents = String::new();
    std::io::Read::read_to_string(&mut file, &mut contents).map_err(|e| {
        CredentialError::Unreadable {
            path: path.to_path_buf(),
            reason: e.to_string(),
        }
    })?;

    // Trailing newlines are what a text editor leaves behind, not part of
    // the key. Interior whitespace is left alone: silently repairing a
    // malformed key would send a wrong credential and read as an
    // authentication failure.
    let key = contents.trim_end_matches(['\n', '\r']).to_string();
    if key.is_empty() {
        return Err(CredentialError::Empty {
            path: path.to_path_buf(),
        });
    }

    Ok(TigApiKey(key))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    /// A synthetic key in a private directory. Never a real credential: a
    /// test that needed one would only run on a provisioned machine, and
    /// would write a real secret to a temporary file for no reason.
    fn key_file(name: &str, contents: &str, mode: u32) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("tig-cred-{}-{name}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("key");
        std::fs::write(&path, contents).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).unwrap();
        path
    }

    #[test]
    fn loads_a_key_the_gateway_alone_can_read() {
        let path = key_file("ok", "synthetic-key-value\n", 0o600);
        let key = load(&path).expect("0600 is readable only by the owner");
        assert_eq!(key.expose(), "synthetic-key-value");
        assert!(key.is_present());
    }

    #[test]
    fn refuses_a_file_anyone_else_can_read() {
        // §2.2: "The file is readable only by the gateway identity." Group
        // and world are both refused, and the mode is named so an operator
        // can fix it without guessing.
        for mode in [0o644, 0o640, 0o604, 0o666] {
            let path = key_file(&format!("perm{mode:o}"), "synthetic\n", mode);
            match load(&path) {
                Err(CredentialError::TooPermissive { mode: reported, .. }) => {
                    assert_eq!(reported, mode)
                }
                other => panic!("mode {mode:o} must be refused, got {other:?}"),
            }
        }
    }

    #[test]
    fn an_owner_only_executable_bit_is_still_acceptable() {
        // The rule is about who else can read it, not about tidiness.
        let path = key_file("ownerexec", "synthetic\n", 0o700);
        assert!(load(&path).is_ok());
    }

    #[test]
    fn refuses_an_empty_or_missing_file() {
        let path = key_file("empty", "\n", 0o600);
        assert!(matches!(load(&path), Err(CredentialError::Empty { .. })));

        let missing = std::env::temp_dir().join("tig-cred-nope-does-not-exist");
        let _ = std::fs::remove_file(&missing);
        assert!(matches!(
            load(&missing),
            Err(CredentialError::Missing { .. })
        ));
    }

    #[test]
    fn a_real_io_failure_reports_its_reason_without_the_contents() {
        // The Unreadable branch built from a real std::io::Error, not a
        // formatted one: a directory passes the mode and size checks and
        // then fails on read, which is the only case that exercises
        // `e.to_string()` — the path a leak would travel.
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("tig-cred-dir-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();

        match load(&dir) {
            Err(CredentialError::Unreadable { path, reason }) => {
                assert_eq!(path, dir);
                assert!(!reason.is_empty(), "the reason must say what failed");
            }
            other => panic!("expected Unreadable from a real io error, got {other:?}"),
        }
    }

    #[test]
    fn refuses_a_file_far_larger_than_a_key() {
        let path = key_file("huge", &"x".repeat(5000), 0o600);
        assert!(matches!(
            load(&path),
            Err(CredentialError::Unreadable { .. })
        ));
    }

    #[test]
    fn nothing_formats_the_key() {
        // The usual way a secret escapes is a formatting call nobody
        // reviewed, so the type must not have one that reveals anything.
        let path = key_file("fmt", "super-secret-value\n", 0o600);
        let key = load(&path).unwrap();
        let debug = format!("{key:?}");
        assert!(!debug.contains("super-secret-value"), "got: {debug}");
        assert_eq!(debug, "TigApiKey(<redacted>)");

        // Not even the length, which would say which credential is loaded.
        assert!(!debug.contains("18"), "got: {debug}");
    }

    #[test]
    fn an_error_never_carries_the_key() {
        let path = key_file("errtext", "super-secret-value\n", 0o644);
        let message = format!("{}", load(&path).unwrap_err());
        assert!(!message.contains("super-secret-value"), "got: {message}");
    }
}
