#!/usr/bin/env sh
set -eu

# Downloads a release from GitHub and installs it into ~/.local/bin.
# Pin a specific version with BNKLAUNCH_VERSION=v0.1.0.

REPO="borgenk/bnklaunch"

main() {
    platform="$(uname -s)"
    arch="$(uname -m)"

    case "$platform" in
        Linux)
            case "$arch" in
                x86_64 | x86-64 | x64 | amd64)
                    TARGET="x86_64-unknown-linux-gnu" ;;
                *)
                    echo "Error: unsupported architecture: $arch"
                    exit 1 ;;
            esac
            ;;
        *)
            echo "Error: unsupported OS: $platform (bnklaunch is Linux-only)"
            exit 1
            ;;
    esac

    if command -v curl > /dev/null 2>&1; then
        fetch() { curl -fsSL "$1" -o "$2"; }
        fetch_stdout() { curl -fsSL "$1"; }
    elif command -v wget > /dev/null 2>&1; then
        fetch() { wget -q "$1" -O "$2"; }
        fetch_stdout() { wget -q "$1" -O -; }
    else
        echo "Error: curl or wget is required"
        exit 1
    fi

    if [ -n "${BNKLAUNCH_VERSION:-}" ]; then
        VERSION="$BNKLAUNCH_VERSION"
    else
        echo "Resolving latest version..."
        VERSION="$(fetch_stdout "https://api.github.com/repos/${REPO}/releases/latest" \
            | grep '"tag_name"' \
            | head -n1 \
            | cut -d'"' -f4)"
        if [ -z "$VERSION" ]; then
            echo "Error: failed to resolve latest version"
            exit 1
        fi
    fi

    FILENAME="bnklaunch-${VERSION}-${TARGET}.tar.gz"
    URL="https://github.com/${REPO}/releases/download/${VERSION}/${FILENAME}"

    if [ -n "${TMPDIR:-}" ] && [ -d "${TMPDIR}" ]; then
        TMP_DIR="$(mktemp -d "$TMPDIR/bnklaunch-XXXXXX")"
    else
        TMP_DIR="$(mktemp -d "/tmp/bnklaunch-XXXXXX")"
    fi
    trap 'rm -rf "$TMP_DIR"' EXIT

    echo "Downloading bnklaunch ${VERSION} for ${TARGET}..."
    fetch "$URL" "$TMP_DIR/$FILENAME"

    echo "Extracting..."
    tar -xzf "$TMP_DIR/$FILENAME" -C "$TMP_DIR"

    INSTALL_DIR="${HOME}/.local/bin"
    mkdir -p "$INSTALL_DIR"
    mv "$TMP_DIR/bnklaunch" "$INSTALL_DIR/bnklaunch"
    chmod +x "$INSTALL_DIR/bnklaunch"

    echo "Installed bnklaunch ${VERSION} to ${INSTALL_DIR}/bnklaunch"

    if [ "$(command -v bnklaunch)" = "$INSTALL_DIR/bnklaunch" ]; then
        echo "Run with 'bnklaunch', or bind it to a compositor hotkey"
    else
        echo ""
        echo "To run bnklaunch from your terminal, add ~/.local/bin to your PATH:"

        case "${SHELL:-}" in
            *zsh)
                echo "  echo 'export PATH=\$HOME/.local/bin:\$PATH' >> ~/.zshrc"
                echo "  source ~/.zshrc"
                ;;
            *fish)
                echo "  fish_add_path -U \$HOME/.local/bin"
                ;;
            *)
                echo "  echo 'export PATH=\$HOME/.local/bin:\$PATH' >> ~/.bashrc"
                echo "  source ~/.bashrc"
                ;;
        esac
    fi
}

main "$@"
