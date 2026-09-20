//! Slice-2 criterion B9: "the ticket HMAC key stays in the Pool API alone".
//!
//! `security.md` §4.1 stores every enrollment and recovery ticket "only as
//! HMAC-SHA-256 values using a dedicated server key". These are about the
//! key's file and about this process refusing to start without a usable one;
//! `scripts/credential-boundary.sh` proves the *other* half — that no crate
//! outside `pool-api` can load or use it.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::os::unix::fs::PermissionsExt as _;
use std::path::PathBuf;

use pool_config::{Binary, Config, LARGEST_CONFORMING_CONTROL_BODY_BYTES};

struct Scratch {
    dir: PathBuf,
}

impl Scratch {
    fn new(name: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("pool-api-key-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("db-password"), b"local-dev-only").unwrap();
        Self { dir }
    }

    /// A synthetic key in a private file. Never a real secret: a test that
    /// needed one would only run on a provisioned machine, and would write a
    /// real key to a temporary file for no reason.
    fn key_file(&self, name: &str, contents: &[u8], mode: u32) -> PathBuf {
        let path = self.dir.join(name);
        std::fs::write(&path, contents).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).unwrap();
        path
    }

    fn config(&self, key_file: &std::path::Path) -> Config {
        let password_file = self.dir.join("db-password").display().to_string();
        let path = self.dir.join("config.toml");
        std::fs::write(
            &path,
            format!(
                r#"
network = "testnet"

[database]
host = "127.0.0.1"
port = 5433
name = "pool_dev"
user = "pool_api"
password_file = "{password_file}"
statement_timeout_ms = 30000

[telemetry]
format = "json"
level = "info"
deployment = "test"

[member_api]
listen = "127.0.0.1:8081"
pool_domain = "test.invalid"
login_chain_id = 84532
ticket_hmac_key_file = "{}"
max_control_body_bytes = {LARGEST_CONFORMING_CONTROL_BODY_BYTES}
"#,
                key_file.display()
            ),
        )
        .unwrap();
        Config::load(&path, Binary::PoolApi).expect("a pool-api config loads")
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn preflight(scratch: &Scratch, key_file: &std::path::Path) -> Result<(), String> {
    let config = scratch.config(key_file);
    let api = config.member_api.as_ref().expect("the section");
    pool_api::service::preflight(api)
}

#[test]
fn a_usable_key_lets_this_process_start() {
    let scratch = Scratch::new("ok");
    let key = scratch.key_file("key", &[0x5a; 32], 0o600);
    preflight(&scratch, &key).expect("32 private bytes are a usable key");
}

#[test]
fn a_key_anyone_else_could_read_stops_startup() {
    // The same rule the gateway's key file carries. A key readable by the
    // group is a key held by whatever else runs as that group, which is not
    // "the Pool API alone".
    let scratch = Scratch::new("perms");
    for (name, mode) in [
        ("a", 0o644),
        ("b", 0o640),
        ("c", 0o604),
        ("d", 0o666),
        ("e", 0o660),
    ] {
        // Named without the mode. `key644` would put "644" into the path,
        // and the path is in every message — so "the message names the mode"
        // would pass even if `Display` stopped naming it. That assertion
        // lives in the crate's own tests, where the path can be stripped
        // before looking.
        let key = scratch.key_file(name, &[0x5a; 32], mode);
        preflight(&scratch, &key).expect_err("mode must be refused");
    }

    // And the private modes that are fine, so the check is about other
    // readers rather than about permissions in general.
    for mode in [0o600, 0o400] {
        let key = scratch.key_file(&format!("ok{mode:o}"), &[0x5a; 32], mode);
        preflight(&scratch, &key).unwrap_or_else(|e| panic!("mode {mode:o} must load: {e}"));
    }
}

#[test]
fn a_key_too_short_to_be_one_stops_startup() {
    // Shorter than the digest it keys adds nothing over the hash alone, and
    // a file that short is a mistake rather than a policy.
    let scratch = Scratch::new("short");
    for bytes in [0_usize, 1, 31] {
        let key = scratch.key_file(&format!("k{bytes}"), &vec![0x5a; bytes], 0o600);
        let err = preflight(&scratch, &key).expect_err("must be refused");
        assert!(err.contains("32"), "the message names the minimum: {err}");
    }

    // Exactly the minimum loads, so the rejection is a bound.
    let key = scratch.key_file("k32", &[0x5a; 32], 0o600);
    preflight(&scratch, &key).expect("32 bytes is the minimum, not past it");
}

#[test]
fn a_file_that_is_not_a_key_stops_startup() {
    let scratch = Scratch::new("wrong-file");

    // A pasted certificate, a log, the wrong path entirely.
    let big = scratch.key_file("big", &vec![0x5a; 4097], 0o600);
    let err = preflight(&scratch, &big).expect_err("must be refused");
    assert!(err.contains("wrong file"), "{err}");

    let missing = scratch.dir.join("not-here");
    let err = preflight(&scratch, &missing).expect_err("must be refused");
    assert!(err.contains("no ticket HMAC key file"), "{err}");
}

#[test]
fn a_trailing_newline_is_not_part_of_the_key() {
    // What an editor adds and what `echo` writes. Trimmed from the end only:
    // a key is bytes, and trimming the front would accept two different files
    // as one key.
    let scratch = Scratch::new("newline");

    // 32 bytes plus a newline is still a 32-byte key, not a 33-byte one.
    let mut with_newline = vec![0x5a; 32];
    with_newline.push(b'\n');
    let key = scratch.key_file("trailing", &with_newline, 0o600);
    preflight(&scratch, &key).expect("a trailing newline is not part of the key");

    // And 31 bytes plus a newline is still too short — the trim happens
    // before the length is judged, which is the order that matters.
    let mut short_with_newline = vec![0x5a; 31];
    short_with_newline.push(b'\n');
    let key = scratch.key_file("short-trailing", &short_with_newline, 0o600);
    assert!(
        preflight(&scratch, &key).is_err(),
        "a newline must not make up the length"
    );
}

#[test]
fn a_key_of_raw_bytes_is_refused_rather_than_quietly_shortened() {
    // The regression this test exists for: the loader strips a trailing
    // newline, so a file of raw random bytes ending in `0x0A` — about one in
    // every two hundred and fifty-six — would lose a byte. A 32-byte key
    // would become 31 and be refused as "too short", which tells an operator
    // nothing about what is wrong, and only on some of their runs.
    //
    // The first version of `scripts/dev-db.sh` in this PR generated exactly
    // that: `head -c 32 /dev/urandom`.
    let scratch = Scratch::new("binary");

    // The unlucky file, spelled out: 31 arbitrary bytes and a trailing 0x0A.
    let mut unlucky = vec![0x5a; 31];
    unlucky.push(b'\n');
    let key = scratch.key_file("unlucky", &unlucky, 0o600);
    let err = preflight(&scratch, &key).expect_err("must be refused");
    assert!(
        err.contains("32"),
        "31 printable bytes plus a newline is a short key, and says so: {err}"
    );

    // And a file that is simply not text is refused for being that, rather
    // than being trimmed into something that might pass.
    let mut binary = vec![0x5a; 40];
    binary[7] = 0x00;
    binary[19] = 0xff;
    let key = scratch.key_file("binary", &binary, 0o600);
    let err = preflight(&scratch, &key).expect_err("must be refused");
    assert!(err.contains("not printable text"), "{err}");

    // Including one whose last byte is the newline the trim looks for: the
    // trim runs first, and what remains is still not text.
    let mut binary_newline = vec![0xab; 40];
    binary_newline.push(b'\n');
    let key = scratch.key_file("binary-newline", &binary_newline, 0o600);
    let err = preflight(&scratch, &key).expect_err("must be refused");
    assert!(err.contains("not printable text"), "{err}");
}

#[test]
fn the_key_the_dev_script_generates_is_one_this_process_accepts() {
    // The two halves of this PR agreeing. `scripts/dev-db.sh` writes 44
    // characters of filtered base64; if the loader's rules and the
    // generator's output ever part company, the failure is at an operator's
    // first `pool-api run` and not here.
    let scratch = Scratch::new("dev-shape");
    let generated: Vec<u8> = b"icia1lfzQwErTyUiOpAsDfGhJkLzXcVbNmQwErTyUiOp".to_vec();
    assert_eq!(generated.len(), 44, "the shape dev-db.sh writes");
    let key = scratch.key_file("generated", &generated, 0o600);
    preflight(&scratch, &key).expect("the generated shape is accepted");

    // And with the newline a shell redirect or an editor might leave.
    let mut with_newline = generated;
    with_newline.push(b'\n');
    let key = scratch.key_file("generated-newline", &with_newline, 0o600);
    preflight(&scratch, &key).expect("a trailing newline is still fine");
}

#[test]
fn the_error_never_carries_the_key() {
    // §4.1's whole point: the paths and reasons an operator needs are in the
    // message; the bytes are not, however the file is shaped.
    //
    // What a *loaded* key shows of itself is checked inside the crate
    // (`ticket_key::tests::a_loaded_key_shows_nothing_of_itself`), because no
    // public function returns a `TicketKey` — an integration test cannot
    // obtain one, which is the boundary working.
    let scratch = Scratch::new("no-leak");
    let secret = b"SUPER-SECRET-VALUE-NOBODY-SHOULD-EVER-SEE";

    // Refused for being world-readable, with the secret inside it.
    let key = scratch.key_file("leaky", secret, 0o644);
    let err = preflight(&scratch, &key).expect_err("must be refused");
    assert!(
        !err.contains("SUPER-SECRET"),
        "the error carried the key: {err}"
    );
}
