#!/bin/bash
# 生成用于验证 dmde-rs 的真实文件系统镜像（仅 macOS）。
# 生成后会在每个卷里写入文件、删掉一个文件，然后卸载——这样镜像里既有
# 存活文件也有已删除条目，可以直接拿来测 `ls` / `cat` / `recover`。
#
# 用法: ./scripts/make-test-images.sh [输出目录]
set -euo pipefail

OUT="${1:-/tmp/dmde-test}"
mkdir -p "$OUT"
cd "$OUT"

if ! command -v hdiutil >/dev/null; then
  echo "需要 macOS 的 hdiutil" >&2
  exit 1
fi

# $1=逻辑卷名 $2=文件系统 $3=大小
make_image() {
  local vol="$1" fs="$2" size="$3"
  local name
  name=$(echo "$vol" | tr '[:upper:]' '[:lower:]')
  rm -f "$name.dmg" "${name}raw.dmg"
  # hdiutil 的 -format 需要 srcfolder/srcdevice，所以先建默认格式再 convert
  hdiutil create -size "$size" -fs "$fs" -volname "$vol" "$name.dmg" >/dev/null
  hdiutil convert -format UDRW -o "${name}raw" "$name.dmg" >/dev/null
  echo "已生成 ${name}raw.dmg ($fs)"
}

populate() {
  local vol="$1"
  hdiutil attach -nobrowse "/Volumes/$vol" >/dev/null 2>&1 || true
  local m="/Volumes/$vol"
  for i in $(seq 1 200); do [ -d "$m" ] && break; sleep 0.1; done
  mkdir -p "$m/docs"
  printf 'content of %s\n' "$vol" > "$m/readme.txt"
  printf 'this file will be deleted from %s\n' "$vol" > "$m/docs/gone.txt"
  printf 'a long file name for lfn test\n' > "$m/a very long filename example.txt"
  head -c 7000 /dev/urandom > "$m/docs/rand.bin"
  sync
  rm -f "$m/docs/gone.txt" "$m/a very long filename example.txt"
  sync
  hdiutil detach "$m" >/dev/null
  echo "已填充并删除测试文件: $vol"
}

make_image TESTFAT16 MS-DOS 24m
make_image TESTEXFAT ExFAT 64m
make_image TESTHFS HFS+ 64m

populate TESTFAT16
populate TESTEXFAT
populate TESTHFS

# ext4 用 Android SDK 自带的 mke2fs（macOS 无法挂载 ext4，所以用 -d 直接注入文件）
MKE2FS="${MKE2FS:-$(command -v mke2fs || echo "$HOME/Library/Android/platform-tools/mke2fs")}"
if [ -x "$MKE2FS" ]; then
  rm -rf ext4src && mkdir -p ext4src/docs
  printf 'ext4 hello\n' > ext4src/hello.txt
  printf 'nested file\n' > ext4src/docs/nested.txt
  head -c 3000 /dev/urandom > ext4src/docs/blob.bin
  dd if=/dev/zero of=ext4.img bs=1m count=64 2>/dev/null
  "$MKE2FS" -q -F -t ext4 -L TESTEXT4 -d ext4src ext4.img
  echo "已生成 ext4.img"
else
  echo "跳过 ext4（没找到 mke2fs）"
fi

echo
echo "完成。试试："
echo "  ./target/debug/dmde-rs volumes $OUT/hfsraw.dmg"
echo "  ./target/debug/dmde-rs ls -l $OUT/hfsraw.dmg"
echo "  ./target/debug/dmde-rs recover $OUT/fat16raw.dmg --dry-run"
