#!/usr/bin/env bash
# 在 atomdns 上游代码上打「UDP 回包源地址」补丁，编出可在 Debian 12 直接跑的静态二进制。
# 用法：./build.sh            # 用上游默认分支
#       ./build.sh v0.6.1     # 用指定 tag
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PATCH="$HERE/patches/0001-udp-reply-source-address.patch"
WORK="${WORK:-$HERE/work}"
REF="${1:-}"
TARGET="x86_64-unknown-linux-musl"

if [[ ! -d "$WORK/.git" ]]; then
  echo "== 克隆上游到 $WORK"
  git clone https://github.com/jasonxtt/atomdns "$WORK"
else
  echo "== 更新上游"
  git -C "$WORK" fetch --depth 1 origin "${REF:-HEAD}"
  git -C "$WORK" checkout -f FETCH_HEAD
fi

if [[ -n "$REF" ]]; then
  echo "== 切到 $REF"
  git -C "$WORK" fetch --depth 1 origin "refs/tags/$REF:refs/tags/$REF" 2>/dev/null || git -C "$WORK" fetch --depth 1 origin "$REF"
  git -C "$WORK" checkout -f "$REF"
fi

UPSTREAM_REV="$(git -C "$WORK" rev-parse --short HEAD)"
echo "== 上游版本 $UPSTREAM_REV，套补丁"

if git -C "$WORK" apply --check "$PATCH" 2>/dev/null; then
  git -C "$WORK" apply "$PATCH"
elif git -C "$WORK" apply --check --3way "$PATCH" 2>/dev/null; then
  git -C "$WORK" apply --3way "$PATCH"
elif git -C "$WORK" diff --quiet && git -C "$WORK" apply --reverse --check "$PATCH" 2>/dev/null; then
  echo "== 补丁已在工作区，跳过"
else
  echo "!! 补丁套不上：上游改动了收发路径，需要人工 rebase" >&2
  echo "   手工处理：cd $WORK && git apply --3way $PATCH，解冲突后" >&2
  echo "   git -C $WORK diff > $PATCH" >&2
  exit 2
fi

export ATOMDNS_BUILD_VERSION="vip-$UPSTREAM_REV"
echo "== 编译 $TARGET"
cd "$WORK/rust"
cargo build --release --target "$TARGET" -p atomdns-native-host

mkdir -p "$HERE/out"
cp "$WORK/rust/target/$TARGET/release/atomdns" "$HERE/out/atomdns"
echo "== 产物 $HERE/out/atomdns"
ls -l "$HERE/out/atomdns"
sha256sum "$HERE/out/atomdns"
