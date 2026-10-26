#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"

RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
BLUE='\033[0;34m'
NC='\033[0m'

MODE="release"
ARCH="$(uname -m)"
CLEAN=false

usage() {
    cat <<'EOF'
Usage: ./scripts/build.sh [--clean] [--release|--dev] [--arch x86_64|aarch64]

Builds togewire and togedemon as statically linked (musl) binaries.

Options:
  --clean          Run cargo clean before building
  --release        Build with the release profile (default)
  --dev            Build with the dev profile (appends -dev to binary names)
  --arch ARCH      Target architecture: x86_64 or aarch64 (default: host arch)
  -h, --help       Show this help message
EOF
}

while [ "$#" -gt 0 ]; do
    case "$1" in
        --clean) CLEAN=true ;;
        --release) MODE="release" ;;
        --dev) MODE="dev" ;;
        --arch)
            shift
            ARCH="${1:?--arch needs x86_64 or aarch64}"
            ;;
        -h|--help) usage; exit 0 ;;
        *) echo "Unknown option: $1" >&2; usage >&2; exit 1 ;;
    esac
    shift
done

# SHA-512 of the musl.cc toolchain tarballs (unchanged since 2021, per musl.cc/SHA512SUMS).
case "$ARCH" in
    x86_64|amd64)
        ARCH="x86_64"
        MUSL_SHA512="52abd1a56e670952116e35d1a62e048a9b6160471d988e16fa0e1611923dd108a581d2e00874af5eb04e4968b1ba32e0eb449a1f15c3e4d5240ebe09caf5a9f3"
        ;;
    aarch64|arm64)
        ARCH="aarch64"
        MUSL_SHA512="8695ff86979cdf30fbbcd33061711f5b1ebc3c48a87822b9ca56cde6d3a22abd4dab30fdcd1789ac27c6febbaeb9e5bde59d79d66552fae53d54cc1377a19272"
        ;;
    *) echo -e "${RED}Invalid arch: $ARCH (must be x86_64 or aarch64)${NC}" >&2; exit 1 ;;
esac

echo -e "${BLUE}Building Togewire (musl, static)...${NC}"

if [ "$CLEAN" = true ]; then
    echo -e "${RED}Cleaning target directory...${NC}"
    find "$ROOT_DIR/target" -mindepth 1 -maxdepth 1 ! -name "musl-toolchains" -exec rm -rf {} + 2>/dev/null || true
fi

TARGET="${ARCH}-unknown-linux-musl"
GCC_NAME="${ARCH}-linux-musl-gcc"
TOOLCHAIN_DIR="$ROOT_DIR/target/musl-toolchains"

# Prefer musl-gcc on PATH; otherwise download a musl.cc cross toolchain.
if ! command -v "$GCC_NAME" >/dev/null 2>&1; then
    mkdir -p "$TOOLCHAIN_DIR"
    if [ ! -d "$TOOLCHAIN_DIR/${ARCH}-linux-musl-cross" ]; then
        echo "Downloading ${ARCH} musl cross compiler..."
        TGZ="$TOOLCHAIN_DIR/${ARCH}-linux-musl-cross.tgz"
        wget -qO "$TGZ" "https://musl.cc/${ARCH}-linux-musl-cross.tgz"
        echo "$MUSL_SHA512  $TGZ" | sha512sum -c --quiet -
        tar -xzf "$TGZ" -C "$TOOLCHAIN_DIR"
        rm -f "$TGZ"
    fi
    export PATH="$TOOLCHAIN_DIR/${ARCH}-linux-musl-cross/bin:$PATH"
fi

TARGET_ENV="$(echo "$TARGET" | tr '[:lower:]-' '[:upper:]_')"
export "CARGO_TARGET_${TARGET_ENV}_LINKER=$GCC_NAME"
export "CC_${TARGET//-/_}=$GCC_NAME"
export "CXX_${TARGET//-/_}=${ARCH}-linux-musl-g++"

rustup target add "$TARGET" >/dev/null 2>&1 || true

PROFILE_DIR="release"
[ "$MODE" = "dev" ] && PROFILE_DIR="debug"

echo -e "${YELLOW}Profile: $MODE | Arch: $ARCH | Target: $TARGET${NC}"

echo -e "${GREEN}Building...${NC}"
cargo build --manifest-path "$ROOT_DIR/Cargo.toml" --target "$TARGET" --profile "$MODE"

# Naming: togewire[-dev][-aarch64], togedemon[-dev][-aarch64]
DEV_SUFFIX=""
ARCH_SUFFIX=""
[ "$MODE" = "dev" ] && DEV_SUFFIX="-dev"
[ "$ARCH" = "aarch64" ] && ARCH_SUFFIX="-aarch64"

BUILD_DIR="$ROOT_DIR/target/$TARGET/$PROFILE_DIR"

for BIN in togewire togedemon; do
    DEST="$ROOT_DIR/${BIN}${DEV_SUFFIX}${ARCH_SUFFIX}"
    # Copy then rename, so a running binary is replaced rather than overwritten.
    cp "$BUILD_DIR/$BIN" "$DEST.tmp"
    mv -f "$DEST.tmp" "$DEST"
    echo -e "${GREEN}${BIN}:${NC}"
    ls -lh "$DEST"
done

echo -e "${GREEN}Done!${NC}"
