//! The dedicated key that enrollment and recovery tickets are hashed under.
//!
//! `security.md` §4.1: "the Pool API stores enrollment and recovery tickets
//! only as HMAC-SHA-256 values using a dedicated server key; it never stores
//! the bearer value." Slice-2 criterion B9 adds where the key may live: "the
//! ticket HMAC key stays in the Pool API alone, so no other process holds
//! it."
//!
//! Same shape as `tig_gateway::credential`, and for the same reasons.
//! [`load`] and [`TicketKey::hmac`] are crate-private, **and no public
//! function returns a [`TicketKey`]** — the second half matters as much as
//! the first, because a public function handing one out is a public way to
//! obtain one however private the loader is. So no other crate can obtain a
//! key or use one — sharing a Rust library does not grant access to
//! the secret, and that is a compile error rather than a convention. The key
//! has no `Display` and a `Debug` that prints nothing of it, because a
//! formatting call is how a secret usually escapes and it is not usually the
//! call anyone reviewed.
//!
//! The file holds printable text — base64 or hex — rather than raw bytes.
//! That is not cosmetic: the loader strips a trailing newline, because that
//! is what an editor adds, and a file of raw random bytes ends in `0x0A`
//! about once in every two hundred and fifty-six. Text and the trim are
//! consistent; binary and the trim are a key that is occasionally one byte
//! shorter than the file, which is the kind of failure that reaches
//! production because it passed every time it was tried.
//!
//! Why keyed at all, rather than a plain SHA-256 of the bearer value: a
//! ticket is 256 bits of entropy, so a plain hash is not guessable — but a
//! database copy would then let its holder confirm a *guessed* ticket
//! offline, and confirm the same ticket against every deployment. The key
//! makes the stored value useless without the file.

use std::path::{Path, PathBuf};

use hmac::{Hmac, Mac as _};
use sha2::Sha256;

/// The ticket HMAC key.
///
/// Deliberately opaque: no `Display`, no revealing `Debug`, no `Serialize`,
/// no `Clone`. The only thing it will do is hash, and that is crate-private.
pub struct TicketKey(Vec<u8>);

impl TicketKey {
    /// The HMAC-SHA-256 of `bearer` under this key: the value stored in
    /// `pool.enrollment_ticket.ticket_hmac`, which is also its lookup key.
    ///
    /// Crate-private, so the key cannot be borrowed to hash something else.
    #[allow(
        dead_code,
        reason = "the ticket-issuing route arrives with the account system"
    )]
    pub(crate) fn hmac(&self, bearer: &[u8]) -> [u8; 32] {
        // `new_from_slice` rejects only a length no HMAC accepts, and HMAC
        // accepts any length; `load` has already refused a short key, which
        // is the bound that matters.
        let mut mac = <Hmac<Sha256>>::new_from_slice(&self.0)
            .unwrap_or_else(|_| unreachable!("HMAC-SHA-256 accepts any key length"));
        mac.update(bearer);
        mac.finalize().into_bytes().into()
    }

    /// Whether a key is present, without revealing anything about it.
    pub fn is_present(&self) -> bool {
        !self.0.is_empty()
    }
}

impl std::fmt::Debug for TicketKey {
    /// Prints no part of the key and not its length: a length distinguishes
    /// one provisioned key from another, and `{:?}` in a tracing field is
    /// exactly the accident §4.1 forbids.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("TicketKey(<redacted>)")
    }
}

#[derive(Debug)]
pub enum TicketKeyError {
    Missing {
        path: PathBuf,
    },
    /// The same rule the gateway's key file carries: readable only by the
    /// identity that holds it.
    TooPermissive {
        path: PathBuf,
        mode: u32,
    },
    /// A key shorter than the digest it produces adds no strength over the
    /// hash itself, and a file that short is a mistake rather than a policy.
    TooShort {
        path: PathBuf,
        bytes: usize,
    },
    /// The file is not text.
    ///
    /// This exists because of the newline trim below. A file of raw random
    /// bytes ends in `0x0A` about once in every two hundred and fifty-six,
    /// and trimming that would take a byte off the key — silently, and only
    /// sometimes. Refusing a non-text file makes the trim mean what it says:
    /// it removes what an editor added, not part of a key.
    NotText {
        path: PathBuf,
    },
    /// A key is short. A larger file is a pasted certificate, a log, or the
    /// wrong path entirely, and reading it in to find out is the thing this
    /// module is trying not to do.
    TooLarge {
        path: PathBuf,
        bytes: u64,
    },
    /// The error text carries the path and the reason, never the contents.
    Unreadable {
        path: PathBuf,
        reason: String,
    },
}

impl std::fmt::Display for TicketKeyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TicketKeyError::Missing { path } => {
                write!(f, "no ticket HMAC key file at {}", path.display())
            }
            TicketKeyError::TooPermissive { path, mode } => write!(
                f,
                "{} is readable beyond the pool-api identity (mode {mode:o}); \
                 security.md §4.1 keeps this key in the Pool API alone",
                path.display()
            ),
            TicketKeyError::TooShort { path, bytes } => write!(
                f,
                "the ticket HMAC key at {} is {bytes} bytes; at least {MIN_KEY_BYTES} \
                 are required, which is the width of the digest it keys",
                path.display()
            ),
            TicketKeyError::NotText { path } => write!(
                f,
                "the ticket HMAC key at {} is not printable text; provision it \
                 as base64 or hex, because a file of raw bytes cannot be told \
                 from one an editor added a newline to",
                path.display()
            ),
            TicketKeyError::TooLarge { path, bytes } => write!(
                f,
                "the ticket HMAC key file at {} is {bytes} bytes; a key is \
                 at most {MAX_KEY_BYTES}, so this is the wrong file",
                path.display()
            ),
            TicketKeyError::Unreadable { path, reason } => write!(
                f,
                "cannot read the ticket HMAC key file at {}: {reason}",
                path.display()
            ),
        }
    }
}

impl std::error::Error for TicketKeyError {}

/// The narrowest key worth having: the width of SHA-256's own output.
const MIN_KEY_BYTES: usize = 32;

/// Bytes a key file may hold. A key is short; a larger file is a pasted
/// certificate or the wrong path entirely.
const MAX_KEY_BYTES: u64 = 4096;

/// Load the ticket key from its file.
///
/// Crate-private on purpose: see the module doc. The `pool-api` binary lives
/// in this crate; nothing else can call this.
pub(crate) fn load(path: &Path) -> Result<TicketKey, TicketKeyError> {
    use std::io::Read as _;

    // Opened first, then checked on the OPEN HANDLE. `metadata(path)`
    // followed by a read is two lookups of one name, and a symlink or
    // permission swap between them defeats the check.
    let mut file = std::fs::File::open(path).map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            TicketKeyError::Missing {
                path: path.to_path_buf(),
            }
        } else {
            TicketKeyError::Unreadable {
                path: path.to_path_buf(),
                reason: e.to_string(),
            }
        }
    })?;

    let metadata = file.metadata().map_err(|e| TicketKeyError::Unreadable {
        path: path.to_path_buf(),
        reason: e.to_string(),
    })?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let mode = metadata.permissions().mode() & 0o777;
        if mode & 0o077 != 0 {
            return Err(TicketKeyError::TooPermissive {
                path: path.to_path_buf(),
                mode,
            });
        }
    }

    if metadata.len() > MAX_KEY_BYTES {
        return Err(TicketKeyError::TooLarge {
            path: path.to_path_buf(),
            bytes: metadata.len(),
        });
    }

    let mut key = Vec::new();
    file.read_to_end(&mut key)
        .map_err(|e| TicketKeyError::Unreadable {
            path: path.to_path_buf(),
            reason: e.to_string(),
        })?;

    // A trailing newline is what an editor adds and what `echo` writes, and
    // it is not part of the key an operator provisioned. Trimmed from the end
    // only: trimming leading bytes would silently accept two different files
    // as the same key.
    while key.last().is_some_and(|b| *b == b'\n' || *b == b'\r') {
        key.pop();
    }

    // And the rest must be text, which is what makes that trim safe. A file
    // of raw random bytes ends in `0x0A` about once in two hundred and
    // fifty-six: trimming it would take a byte off the key, silently and
    // only sometimes — a 32-byte key would become 31 and be refused as short,
    // and a longer one would simply be a different key than the file holds.
    // Requiring printable text means the only thing the loop can remove is
    // the thing it is there to remove.
    if !key.iter().all(|b| (0x21..=0x7e).contains(b)) {
        return Err(TicketKeyError::NotText {
            path: path.to_path_buf(),
        });
    }

    if key.len() < MIN_KEY_BYTES {
        return Err(TicketKeyError::TooShort {
            path: path.to_path_buf(),
            bytes: key.len(),
        });
    }

    Ok(TicketKey(key))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    /// A synthetic key in a private file. Never a real secret.
    fn key_file(name: &str, contents: &[u8], mode: u32) -> PathBuf {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = std::env::temp_dir().join(format!("pool-api-key-unit-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(name);
        std::fs::write(&path, contents).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).unwrap();
        path
    }

    #[test]
    fn a_loaded_key_shows_nothing_of_itself() {
        // In this crate, because no public function returns a `TicketKey` —
        // an integration test could not obtain one, which is the point.
        let secret = b"SUPER-SECRET-VALUE-NOBODY-SHOULD-EVER-SEE";
        let key = load(&key_file("debug", secret, 0o600)).expect("a usable key");

        let debug = format!("{key:?}");
        assert!(!debug.contains("SUPER-SECRET"), "{debug}");
        // Not the length either: a length distinguishes one provisioned key
        // from another.
        assert!(!debug.contains(&secret.len().to_string()), "{debug}");
        assert!(key.is_present());
    }

    #[test]
    fn the_permission_message_names_the_mode_and_not_just_the_path() {
        // The file is named without its mode on purpose. Naming it `key644`
        // would put "644" in the path, and the path is in every message — so
        // an assertion that the message mentions the mode would pass even if
        // `Display` stopped naming it.
        let path = key_file("plain", &[0x5a; 32], 0o644);
        let Err(e) = load(&path) else {
            panic!("0644 must be refused")
        };
        let rendered = e.to_string();
        let without_path = rendered.replace(&path.display().to_string(), "<path>");
        assert!(
            without_path.contains("644"),
            "the mode must survive removing the path: {without_path}"
        );
    }

    #[test]
    fn the_same_key_hashes_the_same_bearer_the_same_way() {
        // `hmac` is crate-private, so this is the only place it can be
        // exercised at all.
        let key = load(&key_file("hmac", &[0x5a; 32], 0o600)).expect("a usable key");
        // Printable, because this crate's own rule requires it — `[0xa5; 32]`
        // is refused as not text, which the first version of this test met.
        let other = load(&key_file("hmac2", &[0x41; 32], 0o600)).expect("a usable key");

        assert_eq!(key.hmac(b"ticket-one"), key.hmac(b"ticket-one"));
        assert_ne!(key.hmac(b"ticket-one"), key.hmac(b"ticket-two"));
        // A different key gives a different value for the same bearer, which
        // is what makes a stored hash useless without the file.
        assert_ne!(key.hmac(b"ticket-one"), other.hmac(b"ticket-one"));
        assert_eq!(key.hmac(b"ticket-one").len(), 32);
    }
}
