//! The dedicated key that enrollment and recovery tickets are hashed under.
//!
//! `security.md` §4.1: "the Pool API stores enrollment and recovery tickets
//! only as HMAC-SHA-256 values using a dedicated server key; it never stores
//! the bearer value." Slice-2 criterion B9 adds where the key may live: "the
//! ticket HMAC key stays in the Pool API alone, so no other process holds
//! it."
//!
//! Same shape as `tig_gateway::credential`, and for the same reasons.
//! [`load`] and [`TicketKey::hmac`] are crate-private, so no other crate can
//! obtain a key or use one — sharing a Rust library does not grant access to
//! the secret, and that is a compile error rather than a convention. The key
//! has no `Display` and a `Debug` that prints nothing of it, because a
//! formatting call is how a secret usually escapes and it is not usually the
//! call anyone reviewed.
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
    // only: a key is bytes, and trimming leading bytes would silently accept
    // two different files as the same key.
    while key.last().is_some_and(|b| *b == b'\n' || *b == b'\r') {
        key.pop();
    }

    if key.len() < MIN_KEY_BYTES {
        return Err(TicketKeyError::TooShort {
            path: path.to_path_buf(),
            bytes: key.len(),
        });
    }

    Ok(TicketKey(key))
}
