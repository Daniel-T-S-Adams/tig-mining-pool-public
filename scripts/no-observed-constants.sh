#!/usr/bin/env bash
# Slice-1 criterion C4: an observed TIG constant must not be compiled into
# slice-1 source.
#
# `docs/tig_integration.md` §12 allows code to carry field names and pinned
# enum values but never "a value copied from the testnet observation as a
# permanent protocol constant". C4 asks for the grep-based check that the
# constants recorded in the spike report do not appear as literals.
#
# WHAT THIS GUARDS, precisely — it does not verify that every §12 value is
# read from the snapshot, which no grep can do:
#
#   1. Values observed on testnet, taken from `docs/protocol_spike_report.md`
#      (integers of five or more digits: the run block heights and
#      `blocks_per_round`). This is the set C4 names. The report is a record
#      of runs that already happened, so extracting from it cannot go stale.
#   2. Values from `fixtures/tig/v1/get-block.json` (four or more digits).
#      Those are *constructed, not captured* — the fixture README is explicit
#      that they are "internally consistent inventions" and that "no value
#      here may be treated as a live protocol constant". They are guarded for
#      that reason, not as observations: an invented fixture value compiled
#      into source is the same §12 violation.
#   3. Any §12 field name assigned a numeric literal, regardless of the
#      number's length. Rules 1 and 2 cannot see `submission_delay = 4`,
#      because a bare `4` collides with all ordinary arithmetic. Field names
#      are safe to take from the fixture: §12 permits them in code, and it is
#      the assignment of a literal that this rule refuses.
#
# One narrow exception list. A value another design doc REQUIRES the pool to
# carry is not a §12 violation, and banning it would make a required check
# fail on correct code. Nothing goes on that list without the citation that
# justifies it.
#
# Scope is `crates/*/src` excluding the pre-build spike: tests and fixtures
# legitimately carry these values, and the spike exists to record them.
#
# Usage: scripts/no-observed-constants.sh [--selftest]
# Exit:  0 = clean, 1 = a constant was found, 2 = usage/setup error.
set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
fixture="${POOL_C4_FIXTURE:-$root/fixtures/tig/v1/get-block.json}"
report="${POOL_C4_REPORT:-$root/docs/protocol_spike_report.md}"
scan_root="${POOL_C4_SCAN_ROOT:-$root/crates}"

for required in "$fixture" "$report"; do
    if [[ ! -f "$required" ]]; then
        echo "no-observed-constants: missing $required" >&2
        exit 2
    fi
done

# Rule 1: what was actually observed.
readarray -t observed < <(grep -oE '\b[0-9]{5,}\b' "$report" | sort -u)

# Rules 2 and 3: the constructed fixture's values and field names.
readarray -t extracted < <(python3 - "$fixture" <<'PY'
import json, sys

block = json.load(open(sys.argv[1]))
block = block.get("block", block)
config = block.get("config")
if config is None:
    print("MISSING_CONFIG")
    raise SystemExit(0)

values, names = set(), set()

def walk(node):
    if isinstance(node, dict):
        for key, value in node.items():
            if isinstance(value, (dict, list)):
                walk(value)
                continue
            if isinstance(value, bool):
                continue
            if isinstance(value, int):
                text = str(value)
            elif isinstance(value, float):
                text = repr(value)
            elif isinstance(value, str) and value.strip().lstrip("-").isdigit():
                text = value.strip()
            else:
                continue
            # Only names that read as protocol config fields. The live config
            # also carries short generic keys (a bare "b" in a nested map),
            # and those collide with ordinary local variables — one matched
            # `b = 0` in unrelated source on the first run of this check.
            if len(key) >= 6 and "_" in key:
                names.add(key)
            if len(text.lstrip("-").replace(".", "")) >= 4:
                values.add(text)
    elif isinstance(node, list):
        for item in node:
            walk(item)

walk(config)
for v in sorted(values):
    print(f"VALUE\t{v}")
for n in sorted(names):
    print(f"NAME\t{n}")
PY
)

if [[ ${#extracted[@]} -eq 0 || "${extracted[0]:-}" == "MISSING_CONFIG" ]]; then
    echo "no-observed-constants: $fixture carries no live config to guard" >&2
    exit 2
fi

values=("${observed[@]}")
names=()
for line in "${extracted[@]}"; do
    case "$line" in
        VALUE*) values+=("${line#VALUE$'\t'}") ;;
        NAME*) names+=("${line#NAME$'\t'}") ;;
    esac
done

# Fields the spike report names but the fixture's config does not carry.
names+=(submission_delay block_started block_active)

# Values and field names another design doc requires the pool to carry.
#
#   8453 / 84532, chain_id — docs/accounting.md §12.5: "The pool uses the
#   configured Base network and verifies `eth_chainId` on every signer/RPC
#   start: Base mainnet is `8453` and Base Sepolia is `84532`." Verifying a
#   chain id against a known public constant means holding that constant.
#   The fixture happens to carry 8453 under `erc20.chain_id`, so without
#   this the check would ban code accounting.md mandates.
#
# Each entry needs a citation like the one above. This is not a place to put
# a value that merely tripped the check.
allowed_values=(8453 84532)
allowed_names=(chain_id)

contains() {
    local needle="$1"; shift
    local item
    for item in "$@"; do
        [[ "$item" == "$needle" ]] && return 0
    done
    return 1
}

filtered_values=()
for value in "${values[@]}"; do
    contains "$value" "${allowed_values[@]}" || filtered_values+=("$value")
done
values=("${filtered_values[@]}")

filtered_names=()
for name in "${names[@]}"; do
    contains "$name" "${allowed_names[@]}" || filtered_names+=("$name")
done
names=("${filtered_names[@]}")

if [[ ${#values[@]} -eq 0 || ${#names[@]} -eq 0 ]]; then
    echo "no-observed-constants: nothing extracted; a source file changed shape" >&2
    exit 2
fi

readarray -t sources < <(
    find "$scan_root" -path '*/src/*' -name '*.rs' -not -path '*/spike/*' -print | sort
)
if [[ ${#sources[@]} -eq 0 ]]; then
    echo "no-observed-constants: no slice-1 source found under $scan_root" >&2
    exit 2
fi

# A value goes into a regex, so its metacharacters must be escaped: the
# fixture carries `100.0`, and an unescaped dot is a wildcard that matches
# unrelated five-digit literals like 10050 and fails a required check
# spuriously.
#
# Rust numeric literals may carry `_` separators, so the digits are joined by
# an optional underscore rather than stripping underscores from the source
# first — stripping conflates a separator with an identifier's underscores
# and makes `sha_1270237` look like the literal.
pattern_for() {
    local value="$1" out="" char escaped
    for ((i = 0; i < ${#value}; i++)); do
        char="${value:i:1}"
        escaped="$(printf '%s' "$char" | sed -e 's/[][(){}.*+?^$|\\]/\\&/g')"
        if [[ -n "$out" ]]; then
            out+="_?"
        fi
        out+="$escaped"
    done
    # The right-hand side has to allow a Rust type suffix. `1270241u64` and
    # `10080_u32` are ordinary ways to write these literals, and a boundary
    # demanding a non-word character straight after the digits misses every
    # one of them — which is most of the ways the value would actually be
    # written. The suffix is spelled out rather than allowed as "any word
    # characters", so `sha1270237abcdef` still does not match.
    printf '(^|[^0-9A-Za-z._])%s_?(u8|u16|u32|u64|u128|usize|i8|i16|i32|i64|i128|isize|f32|f64)?([^0-9A-Za-z._]|$)' "$out"
}

patterns=()
for value in "${values[@]}"; do
    patterns+=("$(pattern_for "$value")")
done

# Strip // and /* */ comments while leaving string literals intact.
strip_comments() {
    awk '
    BEGIN { inblk = 0 }
    {
        line = $0; out = ""; i = 1; n = length(line); instr = 0
        while (i <= n) {
            c = substr(line, i, 1); d = substr(line, i, 2)
            if (inblk) {
                if (d == "*/") { inblk = 0; i += 2 } else { i += 1 }
                continue
            }
            if (instr) {
                if (c == "\\") { out = out c substr(line, i + 1, 1); i += 2; continue }
                if (c == "\"") { instr = 0 }
                out = out c; i += 1; continue
            }
            if (c == "\"") { instr = 1; out = out c; i += 1; continue }
            if (d == "//") { i = n + 1; continue }
            if (d == "/*") { inblk = 1; i += 2; continue }
            out = out c; i += 1
        }
        print out
    }' "$1"
}

findings=0

for source in "${sources[@]}"; do
    # Comments are prose. A doc comment naming a value explains why it is
    # not compiled in, which is the opposite of the violation — so stripping
    # them is what stops the check failing on its own explanation.
    #
    # Character-wise rather than line-wise: skipping only lines that START
    # with `//` left a block comment, a `*`-continuation line and a trailing
    # comment all able to fail a required check on prose. String literals
    # are preserved, so a `//` inside a URL does not swallow the rest of the
    # line — and a value hard-coded inside a string is still code, and still
    # caught.
    body="$(strip_comments "$source")"
    [[ -n "$body" ]] || continue

    for i in "${!values[@]}"; do
        if printf '%s\n' "$body" | grep -qE "${patterns[i]}"; then
            echo "HARD-CODED: TIG value ${values[i]} appears in $source"
            findings=$((findings + 1))
        fi
    done

    for name in "${names[@]}"; do
        # Case-insensitive, and tolerant of a type annotation between the
        # name and the literal. `const MAX_FUEL_BUDGET: u64 = 5;` is how a
        # Rust constant is conventionally written, and both the casing and
        # the `: u64 =` slipped past an earlier version that only matched
        # `name = 5` and `name: 5` in lower case — the same shape as the
        # type-suffix miss, one rule over.
        # Two forms. Assignment takes any literal. Comparison — `if
        # precommit.submission_delay == 4` embeds the constant just as much
        # as assigning it — takes any literal EXCEPT 0 and 1, because
        # `bundles > 0` and `count >= 1` are emptiness tests rather than
        # protocol constants, and flagging those would fail a required check
        # on ordinary code.
        assign="\b${name}\b[[:space:]]*[:=][[:space:]]*([A-Za-z0-9_:<>]+[[:space:]]*=[[:space:]]*)?[0-9]"
        compare="\b${name}\b[[:space:]]*(==|!=|<=|>=|<|>)[[:space:]]*([2-9][0-9]*|[0-9]{2,})"
        if printf '%s\n' "$body" | grep -qiE "${assign}|${compare}"; then
            echo "HARD-CODED: §12 field $name is assigned a literal in $source"
            findings=$((findings + 1))
        fi
    done
done

if [[ "${1:-}" == "--selftest" ]]; then
    # Positive control. A check that cannot find anything reports "clean"
    # for exactly the same reason a clean tree does.
    probe="$(mktemp -d)"
    trap 'rm -rf "$probe"' EXIT
    mkdir -p "$probe/planted/src"

    # An observed height (rule 1), a field assigned a literal (rule 3), and
    # a clean file. Each is required to behave, so a selftest cannot pass on
    # one rule while another is broken.
    printf 'pub const ANCHOR: u64 = %s;\n' "${observed[0]}" > "$probe/planted/src/lib.rs"
    if POOL_C4_SCAN_ROOT="$probe" "$0" > /dev/null 2>&1; then
        echo "FAIL: a planted observed constant was not detected" >&2
        exit 1
    fi

    # The same value with a Rust type suffix, which is how it would usually
    # be written. An earlier boundary demanded a non-word character straight
    # after the digits and missed every suffixed literal.
    printf 'pub const ANCHOR: u64 = %su64;\n' "${observed[0]}" > "$probe/planted/src/lib.rs"
    if POOL_C4_SCAN_ROOT="$probe" "$0" > /dev/null 2>&1; then
        echo "FAIL: a planted suffixed constant was not detected" >&2
        exit 1
    fi

    printf 'pub fn f() { let submission_delay = 4; let _ = submission_delay; }\n' \
        > "$probe/planted/src/lib.rs"
    if POOL_C4_SCAN_ROOT="$probe" "$0" > /dev/null 2>&1; then
        echo "FAIL: a planted field assignment was not detected" >&2
        exit 1
    fi

    # The conventional Rust constant: upper case, with a type annotation.
    printf 'pub const SUBMISSION_DELAY: u64 = 4;\n' > "$probe/planted/src/lib.rs"
    if POOL_C4_SCAN_ROOT="$probe" "$0" > /dev/null 2>&1; then
        echo "FAIL: a planted const declaration was not detected" >&2
        exit 1
    fi

    # Compared against rather than assigned, which embeds it just the same.
    printf 'pub fn f(d: u64) -> bool { let submission_delay = d; submission_delay == 4 }\n' \
        > "$probe/planted/src/lib.rs"
    if POOL_C4_SCAN_ROOT="$probe" "$0" > /dev/null 2>&1; then
        echo "FAIL: a planted comparison was not detected" >&2
        exit 1
    fi

    # ...but an emptiness test is ordinary code, not a protocol constant.
    printf 'pub fn f(d: u64) -> bool { let min_num_bundles = d; min_num_bundles > 0 }\n' \
        > "$probe/planted/src/lib.rs"
    if ! POOL_C4_SCAN_ROOT="$probe" "$0" > /dev/null 2>&1; then
        echo "FAIL: an emptiness test was reported as a hard-coded constant" >&2
        exit 1
    fi

    # Guards the regex escaping: with an unescaped `.`, the fixture's 100.0
    # matches this and the check fails a clean file.
    # Clean source that deliberately contains every false-positive shape
    # this check has had: the 10050 that an unescaped `.` once matched, and
    # prose citing a guarded value in each of the three comment forms.
    {
        printf 'pub const UNRELATED: u64 = 10050;\n'
        printf '/* the spike anchored at %s */\n' "${observed[0]}"
        printf '// anchor %s is read from the snapshot\n' "${observed[0]}"
        printf 'pub fn f(x: u64) -> u64 { x + 1 } // anchor %s\n' "${observed[0]}"
    } > "$probe/planted/src/lib.rs"
    if ! POOL_C4_SCAN_ROOT="$probe" "$0" > /dev/null 2>&1; then
        echo "FAIL: clean source was reported as hard-coding a constant" >&2
        exit 1
    fi

    echo "no-observed-constants selftest: detects a planted value, a suffixed value and a field, and passes clean source"
    exit 0
fi

if [[ $findings -gt 0 ]]; then
    echo "no-observed-constants: FAILED with $findings finding(s)" >&2
    echo "§12: read these from the snapshot; never compile in an observed value." >&2
    exit 1
fi

echo "no-observed-constants: clean — ${#sources[@]} file(s) checked against ${#values[@]} value(s) and ${#names[@]} field name(s); ${#allowed_values[@]} value(s) and ${#allowed_names[@]} field(s) are documented exceptions"
