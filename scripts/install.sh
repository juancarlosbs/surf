#!/bin/sh
# Install the latest surf release: curl -fsSL https://raw.githubusercontent.com/juancarlosbs/surf/main/scripts/install.sh | sh
# env: SURF_INSTALL_DIR (default ~/.local/bin), SURF_VERSION (tag, default latest), SURF_DOWNLOAD_BASE (mirror/testing)
set -eu

repo=juancarlosbs/surf
install_dir=${SURF_INSTALL_DIR:-$HOME/.local/bin}

case "$(uname -s)-$(uname -m)" in
    Darwin-arm64) target=aarch64-apple-darwin ;;
    Darwin-x86_64) target=x86_64-apple-darwin ;;
    Linux-x86_64) target=x86_64-unknown-linux-musl ;;
    Linux-aarch64 | Linux-arm64) target=aarch64-unknown-linux-musl ;;
    *)
        echo "surf: no prebuilt binary for $(uname -s)-$(uname -m); build from source with: cargo install --git https://github.com/$repo" >&2
        exit 1
        ;;
esac

if [ -n "${SURF_DOWNLOAD_BASE:-}" ]; then
    base=$SURF_DOWNLOAD_BASE
elif [ -n "${SURF_VERSION:-}" ]; then
    base=https://github.com/$repo/releases/download/$SURF_VERSION
else
    base=https://github.com/$repo/releases/latest/download
fi

archive=surf-$target.tar.gz
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

echo "surf: downloading $base/$archive"
curl -fsSL "$base/$archive" -o "$tmp/$archive"
curl -fsSL "$base/$archive.sha256" -o "$tmp/$archive.sha256"

if command -v sha256sum >/dev/null 2>&1; then
    verify="sha256sum -c"
else
    verify="shasum -a 256 -c"
fi
(cd "$tmp" && $verify "$archive.sha256" >/dev/null 2>&1) || {
    echo "surf: checksum mismatch for $archive; refusing to install" >&2
    exit 1
}

tar -xzf "$tmp/$archive" -C "$tmp"
mkdir -p "$install_dir"
install -m 755 "$tmp/surf-$target/surf" "$install_dir/surf"
echo "surf: installed $("$install_dir/surf" --version) to $install_dir/surf"

case ":$PATH:" in
    *":$install_dir:"*) ;;
    *) echo "surf: add $install_dir to your PATH" ;;
esac
