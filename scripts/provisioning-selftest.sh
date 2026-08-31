#!/usr/bin/env bash
# Proves the psql `\set` quoting convention that provisioning depends on.
#
# A password containing a single quote or a backslash must survive
# provisioning byte for byte. If it does not, the role's real password stops
# matching its password file — and because the A4 scan reads its needle from
# that file, a clean scan silently stops being evidence for that role.
#
# Uses a throwaway role, so no provisioned credential is touched. Runs the
# same way locally and in CI: the only input is a superuser URL.
#   POOL_A4_SUPERUSER_URL   default: the local dev cluster from dev-db.sh
#   POOL_A4_DB              database to connect to (default pool_dev)
set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
image="$(sed -n 's/^image="\(.*\)"$/\1/p' "$root/scripts/dev-db.sh" | head -1)"
role="pool_quote_selftest_$$"

if [[ -n "${POOL_A4_SUPERUSER_URL:-}" ]]; then
    superuser_url="$POOL_A4_SUPERUSER_URL"
else
    if [[ ! -s "$root/secrets/db-superuser-password" ]]; then
        echo "provisioning-selftest: no POOL_A4_SUPERUSER_URL and no local cluster; run make db-up" >&2
        exit 2
    fi
    # Passwordless by construction; the password is read below.
    superuser_url="postgres://postgres@127.0.0.1:5433/postgres"
fi
base="${superuser_url%/*}"
# Default to whatever database the URL already names, rather than one a
# later step happens to create: this check only needs a role and a login,
# not any particular schema, and depending on a database created downstream
# made it fail in CI purely on step ordering.
db="${POOL_A4_DB:-${superuser_url##*/}}"
target_url="$base/$db"
hostport="${target_url#*@}"; hostport="${hostport%%/*}"

# Every character that has ever caused trouble in a shell, psql or SQL layer.
# The backslash matters most: psql processes escapes inside a quoted
# meta-command argument, so a raw one is silently swallowed.
#
# The random suffix matters too: part 2 sets this as a real LOGIN role's
# password, so a fixed literal in this repository would be a working
# credential on any cluster where cleanup failed to drop the role.
password="q'uote\"dq\\back\$dollar;semi--dash-$(head -c 12 /dev/urandom | base64 | tr -d '/+=')"

# Same loopback guard as scripts/a4-scan.sh, which consumes the same entry
# point. This script creates a cluster-wide LOGIN role with a password using
# superuser rights, and only warns if the DROP fails — so against a shared or
# deployed cluster a failed cleanup leaves a live login behind. Both
# consumers of POOL_A4_SUPERUSER_URL must fail closed on the same condition.
scan_host="${target_url#*@}"
scan_host="${scan_host%%:*}"
scan_host="${scan_host%%/*}"
case "$scan_host" in
    127.0.0.1|localhost|::1|"[::1]") ;;
    *)
        if [[ "${POOL_A4_ALLOW_REMOTE:-}" != "1" ]]; then
            echo "provisioning-selftest: refusing to run against non-loopback host '$scan_host'." >&2
            echo "  This creates a login role on that cluster." >&2
            echo "  Set POOL_A4_ALLOW_REMOTE=1 only for a throwaway cluster you own." >&2
            exit 2
        fi
        echo "provisioning-selftest: WARNING running against non-loopback host '$scan_host'" >&2
        ;;
esac

# Discrete connection parameters via the env file; no password-bearing URL
# ever reaches psql's argv (architecture.md §9).
conn_rest="${target_url#*://}"
conn_userinfo="${conn_rest%%@*}"
conn_hostpart="${conn_rest#*@}"
conn_user="${conn_userinfo%%:*}"
# The superuser password comes from the untracked secret file, not from the
# URL. architecture.md §9 excludes secrets from "ordinary environment dumps",
# and POOL_A4_SUPERUSER_URL is an environment variable — the same reason
# POOL_TEST_SUPERUSER_URL is passwordless. An embedded password is still
# honoured so an existing setup keeps working, but it is not the supported
# path and is warned about.
conn_pass=""
if [[ "$conn_userinfo" == *:* ]]; then
    conn_pass="${conn_userinfo#*:}"
    echo "WARNING: POOL_A4_SUPERUSER_URL carries a password; prefer secrets/db-superuser-password" >&2
elif [[ -s "$root/secrets/db-superuser-password" ]]; then
    conn_pass="$(cat "$root/secrets/db-superuser-password")"
fi
conn_host="${conn_hostpart%%:*}"
conn_portdb="${conn_hostpart#*:}"
conn_port="${conn_portdb%%/*}"

run_psql() {
    local ef; ef="$(mktemp)"; chmod 600 "$ef"
    trap 'rm -f "$ef"' RETURN
    {
        printf 'PGHOST=%s\n' "$conn_host"
        printf 'PGPORT=%s\n' "$conn_port"
        printf 'PGUSER=%s\n' "$conn_user"
        printf 'PGDATABASE=%s\n' "$db"
        [[ -n "$conn_pass" ]] && printf 'PGPASSWORD=%s\n' "$conn_pass"
    } > "$ef"
    docker run --rm -i --network host --env-file "$ef" "$image" \
        psql -q -v ON_ERROR_STOP=1 "$@"
}

cleanup() {
    # `|| true` so cleanup never masks the real exit status, but stderr is
    # NOT discarded: a failed DROP leaves a live login role behind, and that
    # has to be visible rather than silent.
    printf 'DROP ROLE IF EXISTS %s;\n' "$role" | run_psql >/dev/null || {
        echo "WARNING: could not drop selftest role $role; drop it manually" >&2
    }
}
trap cleanup EXIT

# ---------------------------------------------------------------------------
# Part 1: the quoting mechanism itself. Round-trip the value through the same
# `\set` -> :'name' path provisioning uses and compare it byte for byte.
#
# This is the real subject of the test and it works under ANY authentication
# mode, which matters because CI runs with trust auth where part 2 cannot
# tell a right password from a wrong one.
esc="$password"
esc="${esc//\\/\\\\}"   # backslashes first
esc="${esc//\'/\'\'}"     # then single quotes

observed="$({
    printf "\\\\set pw '%s'\n" "$esc"
    printf "SELECT :'pw';\n"
} | run_psql -tA)"

if [[ "$observed" != "$password" ]]; then
    # Neither value is a real credential, so printing them is safe and is
    # the only way to make a mismatch diagnosable.
    echo "FAIL: the value did not survive psql \\set quoting" >&2
    echo "  expected: [$password]" >&2
    echo "  observed: [$observed]" >&2
    exit 1
fi
echo "provisioning selftest: value round-trips through \\set verbatim"

# ---------------------------------------------------------------------------
# Part 2: the same value really becomes the role's password. Only meaningful
# where the server actually checks passwords.
printf 'DROP ROLE IF EXISTS %s; CREATE ROLE %s LOGIN;\n' "$role" "$role" \
    | run_psql >/dev/null

{
    printf "\\\\set pw '%s'\n" "$esc"
    printf "ALTER ROLE %s WITH PASSWORD :'pw';\n" "$role"
} | run_psql >/dev/null

auth_as() {
    local pw="$1"
    local ef; ef="$(mktemp)"; chmod 600 "$ef"
    trap 'rm -f "$ef"' RETURN
    printf 'PGPASSWORD=%s\n' "$pw" > "$ef"
    docker run --rm --network host --env-file "$ef" "$image" \
        psql -h "${hostport%%:*}" -p "${hostport##*:}" -U "$role" -d "$db" \
        -tAc "SELECT 1" >/dev/null 2>&1
}

# Establish whether authentication means anything here before asserting on
# it. Under trust auth (CI) a wrong password succeeds, so a passing auth
# check would be evidence of nothing.
if auth_as "definitely-not-the-password"; then
    echo "provisioning selftest: authentication half skipped — this server accepts any password (trust auth)"
    exit 0
fi

if ! auth_as "$password"; then
    echo "FAIL: the round-tripped password was not accepted for the role" >&2
    exit 1
fi

echo "provisioning selftest: the same value authenticates as the role's password"
