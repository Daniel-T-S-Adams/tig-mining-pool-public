#!/usr/bin/env bash
# Slice-1 criterion A4, the real half: run the actual binary against a real
# database, capture everything it emits, dump every value the database
# holds, and prove no provisioned secret appears in either.
#
# `scripts/secret-scan.sh --selftest` proves the scanner can detect a
# planted secret. This proves there is nothing to detect. Both are needed:
# either alone is satisfied by a scanner that does nothing.
#
# Works the same locally and in CI. The only input is a superuser URL:
#   POOL_A4_SUPERUSER_URL   default: the local dev cluster from dev-db.sh
#   POOL_A4_DB              database to migrate and dump (default pool_dev)
#
# psql and pg_dump run from the pinned postgres image rather than whatever
# client the host happens to have, so the dump is always version-matched to
# the server.
set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
# Pinned by digest, not just tag (architecture.md §1): the workflow,
# the dev cluster and the A4 evidence run must all attest to the same
# image, or the evidence describes something CI never ran.
image="postgres:18.4-trixie@sha256:3a82e1f56c8f0f5616a11103ac3d47e632c3938698946a7ad26da0df1334744a"
artifacts="$root/target/a4"
db="${POOL_A4_DB:-pool_dev}"

if [[ -n "${POOL_A4_SUPERUSER_URL:-}" ]]; then
    superuser_url="$POOL_A4_SUPERUSER_URL"
else
    if [[ ! -s "$root/secrets/db-superuser-password" ]]; then
        echo "a4-scan: no POOL_A4_SUPERUSER_URL and no local dev cluster; run make db-up" >&2
        exit 2
    fi
    # Passwordless by construction; the password is read below.
    superuser_url="postgres://postgres@127.0.0.1:5433/postgres"
fi

# Strip any database component and re-point at the target database.
base="${superuser_url%/*}"
target_url="$base/$db"

roles=(migration controller gateway api artifact_worker readonly)

mkdir -p "$root/secrets"
chmod 700 "$root/secrets"
for role in "${roles[@]}"; do
    file="$root/secrets/db-${role//_/-}-password"
    if [[ ! -s "$file" ]]; then
        printf '%s' "$(head -c 32 /dev/urandom | base64 | tr -d '/+=' | head -c 32)" > "$file"
        chmod 600 "$file"
    fi
done

# Refuse a non-loopback target unless explicitly opted in.
#
# What follows resets all six pool role passwords cluster-wide and runs
# `pool-admin migrate`. Pointed at a shared or deployed cluster that would
# overwrite every provisioned service credential and migrate a deployed
# environment — which CLAUDE.md makes a human-only action. The test suite was
# already fixed to stop clobbering cluster-wide roles; this is the same
# hazard in the script, and the fix was not carried across.
scan_host="${target_url#*@}"
scan_host="${scan_host%%:*}"
scan_host="${scan_host%%/*}"
case "$scan_host" in
    127.0.0.1|localhost|::1|"[::1]") ;;
    *)
        if [[ "${POOL_A4_ALLOW_REMOTE:-}" != "1" ]]; then
            echo "a4-scan: refusing to run against non-loopback host '$scan_host'." >&2
            echo "  This resets every pool role password and applies migrations." >&2
            echo "  Set POOL_A4_ALLOW_REMOTE=1 only for a throwaway cluster you own." >&2
            exit 2
        fi
        echo "a4-scan: WARNING running against non-loopback host '$scan_host' (POOL_A4_ALLOW_REMOTE=1)" >&2
        ;;
esac

# Split the superuser URL into discrete parts so nothing password-bearing
# is ever handed to psql as an argument. An earlier version interpolated the
# whole URL into psql's command line inside the container, which put the
# password in that process's argv — visible via /proc from the host, since
# Docker's PID namespace is nested inside it. Same rule, same mistake as the
# grep needle: architecture.md §9 excludes secrets from argv, full stop.
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
conn_db="${conn_hostpart##*/}"

# Every connection parameter, secrets included, travels in the env file.
conn_env_file() {
    local ef; ef="$(mktemp)"; chmod 600 "$ef"
    {
        printf 'PGHOST=%s\n' "$conn_host"
        printf 'PGPORT=%s\n' "$conn_port"
        printf 'PGUSER=%s\n' "$conn_user"
        printf 'PGDATABASE=%s\n' "$1"
        [[ -n "$conn_pass" ]] && printf 'PGPASSWORD=%s\n' "$conn_pass"
    } > "$ef"
    printf '%s' "$ef"
}

run_psql() {
    local database="$1"; shift
    local ef; ef="$(conn_env_file "$database")"
    trap 'rm -f "$ef"' RETURN
    docker run --rm -i --network host --env-file "$ef" "$image" \
        psql -q -v ON_ERROR_STOP=1 "$@"
}

# Create the target database if it is not there yet (CI starts empty).
if ! printf 'SELECT 1 FROM pg_database WHERE datname = %s;\n' "'$db'" \
    | run_psql postgres -tA | grep -q 1; then
    printf 'CREATE DATABASE "%s";\n' "$db" | run_psql postgres
    echo "a4-scan: created database $db"
fi

echo "a4-scan: provisioning roles"
run_psql "$db" < "$root/scripts/provision-db-roles.sql"

{
    for role in "${roles[@]}"; do
        # Double any single quote before interpolating. psql quotes the
        # value on substitution (:'pw_x'), but the `\set` line itself is
        # built here and psql does not escape it.
        #
        # The failure this prevents is a CORRUPTED VALUE, not SQL injection:
        # an unescaped quote makes psql warn "unterminated quoted string"
        # and store a mangled password, so the role's real password stops
        # matching the file — which also silently invalidates the A4 scan,
        # whose needle is read from that file. Verified against psql:
        # `\set x 'a''b'` yields `a'b`, and the unescaped form yields
        # `pw;CREATETABLE...` with no SQL executed.
        pw="$(cat "$root/secrets/db-${role//_/-}-password")"
        # Backslashes FIRST, then quotes. psql processes backslash escapes
        # inside a quoted meta-command argument, so a raw `\` is swallowed
        # (`back\slash` arrives as `backslash`); doubling it preserves it.
        # Doing quotes first would then double the backslashes this step
        # introduces.
        pw="${pw//\\/\\\\}"
        pw="${pw//\'/\'\'}"
        printf "\\\\set pw_%s '%s'\n" "$role" "$pw"
    done
    unset pw
    cat "$root/scripts/provision-db-passwords.sql"
} | run_psql "$db"

# The migration role needs to create the schema the first time.
printf 'GRANT CREATE ON DATABASE "%s" TO pool_migration;\n' "$db" | run_psql "$db"

rm -rf "$artifacts"
mkdir -p "$artifacts"

host="${target_url#*@}"
host="${host%%/*}"
cat > "$artifacts/config.toml" <<TOML
network = "testnet"

[database]
host = "${host%%:*}"
port = "${host##*:}"
name = "$db"
user = "pool_migration"
password_file = "secrets/db-migration-password"
statement_timeout_ms = 30000

[telemetry]
format = "json"
level = "info"
deployment = "a4-scan"
TOML
# TOML wants an integer port, not a quoted string.
sed -i 's/^port = "\(.*\)"$/port = \1/' "$artifacts/config.toml"

echo "a4-scan: running pool-admin migrate with output captured"
if ! cargo run --quiet -p pool-admin -- --config "$artifacts/config.toml" migrate \
        > "$artifacts/run.log" 2>&1; then
    echo "a4-scan: migrate failed" >&2
    cat "$artifacts/run.log" >&2
    exit 1
fi

echo "a4-scan: dumping every value the database holds"
env_file="$(conn_env_file "$db")"
trap 'rm -f "$env_file"' EXIT
docker run --rm --network host --env-file "$env_file" "$image" \
    pg_dump --no-password > "$artifacts/dump.sql"
rm -f "$env_file"

# The role passwords just provisioned are live needles: if any of them
# reached a log line or a database value, this finds it.
"$root/scripts/secret-scan.sh" "$artifacts"
