#!/usr/bin/env bash
# Local development database (docs/architecture.md §11.1).
#
# Starts a PostgreSQL 18 container, generates per-role dev passwords into the
# untracked `secrets/` directory if they are absent, and provisions the
# least-privilege login roles. Idempotent: safe to re-run.
#
# Dev credentials only. This script never touches a deployed environment,
# and the passwords it generates are local-only and untracked.
set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
container="${POOL_PG_CONTAINER:-pool-pg}"
port="${POOL_PG_PORT:-5433}"
# Pinned by digest, not just tag (architecture.md §1): the workflow,
# the dev cluster and the A4 evidence run must all attest to the same
# image, or the evidence describes something CI never ran.
image="postgres:18.4-trixie@sha256:3a82e1f56c8f0f5616a11103ac3d47e632c3938698946a7ad26da0df1334744a"
db="${POOL_PG_DB:-pool_dev}"

roles=(migration controller gateway api artifact_worker readonly)

mkdir -p "$root/secrets"
chmod 700 "$root/secrets"

# One password file per role, generated once, readable only by the owner.
for role in "${roles[@]}"; do
    file="$root/secrets/db-${role//_/-}-password"
    if [[ ! -s "$file" ]]; then
        # No trailing newline: the config loader trims one, but a password
        # file is easier to reason about when it holds exactly the secret.
        printf '%s' "$(head -c 32 /dev/urandom | base64 | tr -d '/+=' | head -c 32)" > "$file"
        chmod 600 "$file"
        echo "generated $file"
    fi
done

if ! docker ps --format '{{.Names}}' | grep -qx "$container"; then
    if docker ps -a --format '{{.Names}}' | grep -qx "$container"; then
        docker start "$container" >/dev/null
    else
        # The superuser password is local-only and lives with the other dev
        # secrets rather than on the command line of a long-lived process.
        super_file="$root/secrets/db-superuser-password"
        if [[ ! -s "$super_file" ]]; then
            printf '%s' "$(head -c 32 /dev/urandom | base64 | tr -d '/+=' | head -c 32)" > "$super_file"
            chmod 600 "$super_file"
        fi
        # Via --env-file, not -e: architecture.md §9 excludes secrets from
        # command-line arguments, and `-e` puts this one in the host process
        # list. It is still visible in `docker inspect` either way — the
        # container needs the variable — so this narrows the exposure rather
        # than removing it. Dev-only, and never a deployed credential.
        env_file="$root/secrets/db-superuser.env"
        umask 077
        {
            printf 'POSTGRES_PASSWORD=%s\n' "$(cat "$super_file")"
            printf 'POSTGRES_DB=%s\n' "$db"
        } > "$env_file"
        docker run -d --name "$container" \
            --env-file "$env_file" \
            -p "127.0.0.1:$port:5432" \
            "$image" >/dev/null
        echo "started $container on 127.0.0.1:$port"
    fi
fi

# Wait for readiness rather than sleeping a guessed interval.
#
# Require several CONSECUTIVE successes. During first-run initialisation the
# image starts a temporary socket-only server, then shuts it down and starts
# the real one, so a single successful probe can be answered by a server
# that is about to disappear.
consecutive=0
ready=0
for _ in $(seq 1 90); do
    if docker exec "$container" pg_isready -q -U postgres 2>/dev/null; then
        consecutive=$((consecutive + 1))
        if [[ $consecutive -ge 3 ]]; then ready=1; break; fi
    else
        consecutive=0
    fi
    sleep 1
done
if [[ $ready -ne 1 ]]; then
    echo "FAIL: $container did not become ready" >&2
    exit 1
fi

# Roles and grants first: plain SQL, no secrets.
docker exec -i "$container" psql -q -v ON_ERROR_STOP=1 -U postgres -d "$db" \
    < "$root/scripts/provision-db-roles.sql"

# Then the passwords, fed on stdin as psql `\set` lines rather than as `-v`
# arguments: argv is visible in the host process list, and architecture.md
# §9 excludes secrets from command-line arguments.
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
} | docker exec -i "$container" psql -q -v ON_ERROR_STOP=1 -U postgres -d "$db"

# The migration role needs to create the schema the first time.
docker exec -i "$container" psql -q -U postgres -d "$db" \
    -c "GRANT CREATE ON DATABASE \"$db\" TO pool_migration;" \
    -c "GRANT CONNECT ON DATABASE \"$db\" TO pool_controller, pool_gateway, pool_api, pool_artifact_worker, pool_readonly;"

echo "dev database ready: postgres://pool_migration@127.0.0.1:$port/$db"
