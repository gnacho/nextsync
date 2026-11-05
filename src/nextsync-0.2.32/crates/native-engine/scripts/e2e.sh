#!/usr/bin/env bash
# End-to-end gate for the native engine against real servers.
#
# Requires: nc-test on $NXS_E2E_NC_URL (default http://127.0.0.1:8081) and
# oc-test on $NXS_E2E_OC_URL (default https://127.0.0.1:9200), both with
# admin/admin1234. Exits non-zero on the first failed check.
set -uo pipefail

BIN="${NXS_E2E_BIN:-/home/nacho/temp/cargo-target/debug/native-engine-cli}"
NC_URL="${NXS_E2E_NC_URL:-http://127.0.0.1:8081}"
OC_URL="${NXS_E2E_OC_URL:-https://127.0.0.1:9200}"
USER_NAME="${NXS_E2E_USER:-admin}"
PASS="${NXS_E2E_PASS:-admin1234}"
REMOTE_DIR="nxs-e2e-$$"
FAILED=0

pass() { printf 'PASS  %s\n' "$*"; }
fail() { printf 'FAIL  %s\n' "$*"; FAILED=1; }
check() { if [ "$1" = "0" ]; then pass "$2"; else fail "$2"; fi; }

# run <provider> <url> <insecure-flag> <local> <remote-path>
run() {
    "$BIN" --provider "$1" --url "$2" --user "$USER_NAME" --password "$PASS" \
        --local "$4" --remote "$5" $3 2>&1
}

e2e() {
    local provider="$1" url="$2" insecure="$3"
    local work; work="$(mktemp -d)"
    local local_dir="$work/local"
    mkdir -p "$local_dir"
    printf 'alpha content\n' > "$local_dir/a.txt"
    printf 'beta content\n' > "$local_dir/b.txt"
    head -c 12582912 /dev/urandom > "$local_dir/big.bin"
    local big_sum; big_sum="$(sha1sum "$local_dir/big.bin" | cut -d' ' -f1)"

    echo "== $provider ($url)"

    out="$(run "$provider" "$url" "$insecure" "$local_dir" "$REMOTE_DIR")"
    echo "$out" | tail -2
    echo "$out" | grep -q 'errors=0'; check $? "$provider: first sync uploads without errors"

    # Idempotent: second pass must not transfer anything.
    out="$(run "$provider" "$url" "$insecure" "$local_dir" "$REMOTE_DIR")"
    echo "$out" | tail -1
    echo "$out" | grep -Eq 'downloaded=0 uploaded=0 deleted_local=0 deleted_remote=0'
    check $? "$provider: second pass is idempotent"

    # Remote integrity of the chunked/TUS upload.
    local remote_url
    if [ "$provider" = "nextcloud" ]; then
        remote_url="$url/remote.php/dav/files/$USER_NAME/$REMOTE_DIR/big.bin"
    else
        if [ -z "${SPACE_ID:-}" ]; then
            SPACE_ID="$(curl -sk -u "$USER_NAME:$PASS" "$url/graph/v1.0/me/drives" \
                | python3 -c 'import sys,json; print(next(d["id"] for d in json.load(sys.stdin)["value"] if d.get("driveType")=="personal"))')"
        fi
        remote_url="$url/dav/spaces/$SPACE_ID/$REMOTE_DIR/big.bin"
    fi
    local remote_sum
    remote_sum="$(curl -sk -u "$USER_NAME:$PASS" "$remote_url" | sha1sum | cut -d' ' -f1)"
    [ "$remote_sum" = "$big_sum" ]; check $? "$provider: uploaded 12 MB is byte-identical remotely"

    # Local deletion propagates.
    rm "$local_dir/a.txt"
    out="$(run "$provider" "$url" "$insecure" "$local_dir" "$REMOTE_DIR")"
    echo "$out" | tail -1
    echo "$out" | grep -q 'deleted_remote=1'; check $? "$provider: local delete propagates to server"

    # Remote creation downloads.
    printf 'gamma from server\n' > "$work/c.txt"
    if [ "$provider" = "nextcloud" ]; then
        curl -sk -u "$USER_NAME:$PASS" -T "$work/c.txt" "$url/remote.php/dav/files/$USER_NAME/$REMOTE_DIR/c.txt" >/dev/null
    else
        curl -sk -u "$USER_NAME:$PASS" -T "$work/c.txt" "$url/dav/spaces/$SPACE_ID/$REMOTE_DIR/c.txt" >/dev/null
    fi
    out="$(run "$provider" "$url" "$insecure" "$local_dir" "$REMOTE_DIR")"
    echo "$out" | tail -1
    [ "$(cat "$local_dir/c.txt" 2>/dev/null)" = "gamma from server" ]
    check $? "$provider: remote file is downloaded with correct content"

    # Conflict keeps a local copy with the name the app recognises.
    printf 'local edit\n' > "$local_dir/b.txt"
    sleep 1
    printf 'remote edit\n' > "$work/b.txt"
    if [ "$provider" = "nextcloud" ]; then
        curl -sk -u "$USER_NAME:$PASS" -T "$work/b.txt" "$url/remote.php/dav/files/$USER_NAME/$REMOTE_DIR/b.txt" >/dev/null
    else
        curl -sk -u "$USER_NAME:$PASS" -T "$work/b.txt" "$url/dav/spaces/$SPACE_ID/$REMOTE_DIR/b.txt" >/dev/null
    fi
    out="$(run "$provider" "$url" "$insecure" "$local_dir" "$REMOTE_DIR")"
    echo "$out" | tail -1
    ls "$local_dir" | grep -q ' (Nextcloud conflicted copy '
    check $? "$provider: conflict produces a Nextcloud-compatible copy name"
    [ "$(cat "$local_dir/b.txt")" = "remote edit" ]
    check $? "$provider: working copy holds the remote version"

    rm -rf "$work"
}

e2e nextcloud "$NC_URL" ""
e2e opencloud "$OC_URL" "--insecure"

if [ "$FAILED" -eq 0 ]; then
    echo "GATE GREEN"
else
    echo "GATE RED"
fi
exit "$FAILED"
