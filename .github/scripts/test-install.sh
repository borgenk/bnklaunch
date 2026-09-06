#!/usr/bin/env sh
set -eu

# The installer's checksum verification, driven over file:// URLs so the real
# downloader runs with no network.

cd "$(dirname "$0")/../.."
ROOT="$(pwd)"

BNKLAUNCH_INSTALL_LIB=1 . ./install.sh

failures=0
check() {
    if [ "$2" = "$3" ]; then
        echo "ok   $1"
    else
        echo "FAIL $1"
        echo "     want: $2"
        echo "     got:  $3"
        failures=$((failures + 1))
    fi
}

TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

select_fetch

mkdir -p "$TMP/remote"
printf 'archive contents\n' > "$TMP/remote/pkg.tar.gz"
good="$(sha256_of "$TMP/remote/pkg.tar.gz")"

# One archive per case, so a fetched digest never lands on another's.
staged() {
    mkdir -p "$TMP/$1"
    cp "$TMP/remote/pkg.tar.gz" "$TMP/$1/pkg.tar.gz"
    printf '%s' "$TMP/$1/pkg.tar.gz"
}

verdict() {
    if verify_checksum "$1" "$2" > /dev/null 2>&1; then echo installs; else echo refuses; fi
}

printf '%s  pkg.tar.gz\n' "$good" > "$TMP/remote/valid.sha256"
check "valid checksum installs" "installs" \
    "$(verdict "$(staged valid)" "file://$TMP/remote/valid.sha256")"

printf '%s  pkg.tar.gz\n' "$(printf '%064d' 0)" > "$TMP/remote/wrong.sha256"
check "mismatch refuses" "refuses" \
    "$(verdict "$(staged wrong)" "file://$TMP/remote/wrong.sha256")"

check "missing checksum refuses" "refuses" \
    "$(verdict "$(staged missing)" "file://$TMP/remote/absent.sha256")"

printf 'not-a-digest  pkg.tar.gz\n' > "$TMP/remote/malformed.sha256"
check "malformed checksum refuses" "refuses" \
    "$(verdict "$(staged malformed)" "file://$TMP/remote/malformed.sha256")"

: > "$TMP/remote/empty.sha256"
check "empty checksum refuses" "refuses" \
    "$(verdict "$(staged empty)" "file://$TMP/remote/empty.sha256")"

mkdir -p "$TMP/nobin"
check "no digest tool refuses" "refuses" \
    "$(PATH="$TMP/nobin" verdict "$(staged notool)" "file://$TMP/remote/valid.sha256")"

check "explicit override installs" "installs" \
    "$(BNKLAUNCH_SKIP_CHECKSUM=1 verdict "$(staged override)" "file://$TMP/remote/absent.sha256")"

# The whole install, with the downloader serving a staged archive off disk. main
# runs in a subshell: it sets its own EXIT trap, and it exits on the paths that
# refuse.
install_run() {
    ( HOME="$1" BNKLAUNCH_VERSION=v9.9.9 main > /dev/null 2>&1 ) \
        && echo installs || echo refuses
}

select_fetch() {
    fetch() {
        case "$1" in
            *.sha256) cp "$REMOTE/pkg.sha256" "$2" ;;
            *) cp "$REMOTE/pkg.tar.gz" "$2" ;;
        esac
    }
}

REMOTE="$TMP/served"
mkdir -p "$REMOTE/stage"
printf '#!/bin/sh\necho launched\n' > "$REMOTE/stage/bnklaunch"
chmod +x "$REMOTE/stage/bnklaunch"
tar czf "$REMOTE/pkg.tar.gz" -C "$REMOTE/stage" .
sha256_of "$REMOTE/pkg.tar.gz" > "$REMOTE/pkg.sha256"

home="$TMP/installed"
check "a verified archive installs" "installs" "$(install_run "$home")"
check "the binary runs from where it landed" "launched" \
    "$("$home/.local/bin/bnklaunch" 2>&1 || echo "did not run")"

# A digest that belongs to nothing, which is what a tampered download looks like.
printf '%064d\n' 0 > "$REMOTE/pkg.sha256"
home="$TMP/tampered"
check "a tampered archive refuses" "refuses" "$(install_run "$home")"
check "and installs nothing" "no" \
    "$([ -e "$home/.local/bin/bnklaunch" ] && echo yes || echo no)"

# An archive holding something else entirely.
sha256_of "$REMOTE/pkg.tar.gz" > "$REMOTE/pkg.sha256"
rm "$REMOTE/stage/bnklaunch"
printf 'not a launcher\n' > "$REMOTE/stage/README"
tar czf "$REMOTE/pkg.tar.gz" -C "$REMOTE/stage" .
sha256_of "$REMOTE/pkg.tar.gz" > "$REMOTE/pkg.sha256"
home="$TMP/empty-archive"
check "an archive without the binary refuses" "refuses" "$(install_run "$home")"
check "and installs nothing" "no" \
    "$([ -e "$home/.local/bin/bnklaunch" ] && echo yes || echo no)"

cd "$ROOT"
if [ "$failures" -ne 0 ]; then
    echo "$failures check(s) failed"
    exit 1
fi
echo "installer checks passed"
