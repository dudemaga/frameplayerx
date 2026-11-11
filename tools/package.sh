#!/usr/bin/env bash
# Builds the FramePlayerX release for the Steam Frame and writes everything
# needed to publish it into dist/:
#
#   dist/frameplayerx-<version>-aarch64.zip   the install archive
#   dist/manifest.json                       update manifest (sign it!)
#   dist/framedrop.json                      FrameDrop manifest
#   dist/frameplayerx.json                    Frame Control install manifest
#
# Usage: tools/package.sh [--url-base URL] [--notes FILE] [--key PREFIX.key]
#   --url-base  where the zip will be downloadable (default: the GitHub
#               release download URL for this version)
#   --key       sign manifest.json with this fp-release key
#
# Needs: the aarch64 FFmpeg build (tools/build-ffmpeg.sh aarch64) and the
# cross toolchain from tools/build-frame.sh.
set -euo pipefail
cd "$(dirname "$0")/.."
root="$PWD"
version="$(cargo metadata --no-deps --format-version 1 | python3 -c 'import json,sys; print(next(p["version"] for p in json.load(sys.stdin)["packages"] if p["name"]=="frameplayerx"))')"
url_base="https://github.com/remotesojourner/frameplayerx/releases/download/v$version"
notes=""
key=""
while [ $# -gt 0 ]; do
  case "$1" in
    --url-base) url_base="$2"; shift 2 ;;
    --notes) notes="$2"; shift 2 ;;
    --key) key="$2"; shift 2 ;;
    *) echo "unknown option $1" >&2; exit 2 ;;
  esac
done

target=aarch64-unknown-linux-gnu
ffmpeg="$root/third_party/ffmpeg/aarch64/lib"
[ -f "$ffmpeg/libavcodec.so" ] || { echo "missing $ffmpeg; run tools/build-ffmpeg.sh aarch64" >&2; exit 1; }

echo "== building FramePlayerX $version for the Steam Frame"
tools/build-frame.sh -p frameplayerx
cargo build --release -p fp-updater --bin fp-release

stage="$root/dist/stage/frameplayerx"
rm -rf "$root/dist/stage" && mkdir -p "$stage/lib" "$stage/licenses" "$stage/assets/steam"
# Cargo already left out the debug info; the symbols stay, so that crash
# reports' backtraces name functions.
cp "target/$target/release/frameplayerx" "$stage/"
for l in avformat.so.61 avcodec.so.61 avfilter.so.10 avutil.so.59 swresample.so.5 swscale.so.8; do
  cp -L "$ffmpeg/lib$l" "$stage/lib/lib$l"
done
cp packaging/frameplayerx.sh packaging/README.txt LICENSE NOTICE "$stage/"
cp packaging/licenses/* "$stage/licenses/"
cp assets/steam/*.png "$stage/assets/steam/"
echo "$version" > "$stage/VERSION"
# The Rust crates compiled into the binary, then the licence, copyright and
# notice files each crate ships, which their licences ask to go with the
# binary. A text several crates share is printed once.
cargo metadata --format-version 1 --filter-platform "$target" | python3 -c '
import json, os, re, sys
m = json.load(sys.stdin)
pkgs = {p["id"]: p for p in m["packages"]}
nodes = {n["id"]: n for n in m["resolve"]["nodes"]}
root = next(i for i, p in pkgs.items() if p["name"] == "frameplayerx")
seen, todo = set(), [root]
while todo:
    i = todo.pop()
    if i in seen: continue
    seen.add(i)
    todo += [d["pkg"] for d in nodes[i]["deps"] if any(k["kind"] in (None, "normal") for k in d["dep_kinds"])]
ours = set(m["workspace_members"])
crates = sorted((pkgs[i] for i in seen if i not in ours), key=lambda p: (p["name"], p["version"]))
named = re.compile(r"(licen[cs]e|copying|copyright|notice|unlicense)", re.I)
texts, bare = {}, []
for p in crates:
    label = "%s %s" % (p["name"], p["version"])
    print("%s: %s" % (label, p.get("license") or "see crate"))
    d = os.path.dirname(p["manifest_path"])
    files = []
    for n in sorted(os.listdir(d)):
        f = os.path.join(d, n)
        if not named.match(n): continue
        if os.path.isfile(f): files.append(f)
        elif os.path.isdir(f):  # LICENSES/, as REUSE lays them out
            files += [os.path.join(f, x) for x in sorted(os.listdir(f)) if os.path.isfile(os.path.join(f, x))]
    if p.get("license_file"):
        lf = os.path.normpath(os.path.join(d, p["license_file"]))
        if lf not in files and os.path.isfile(lf): files.append(lf)
    if not files: bare.append(label)
    for f in files:
        text = open(f, encoding="utf-8", errors="replace").read().strip()
        if label not in texts.setdefault(text, []): texts[text].append(label)
extra = open("packaging/licenses/rust-crates-extra.txt", encoding="utf-8").read()
covered = {c.strip() for l in extra.splitlines() if l.startswith("Crates: ") for c in l[8:].split(",")}
missing = [b for b in bare if b.rsplit(" ", 1)[0] not in covered]
if missing:
    sys.exit("no licence file in %s: add their licences to packaging/licenses/rust-crates-extra.txt" % ", ".join(missing))
if bare:
    print("\nNo licence file of their own, see rust-crates-extra.txt: %s" % ", ".join(bare))
for text, who in sorted(texts.items(), key=lambda t: t[1][0]):
    print("\n%s\n%s\n%s\n\n%s" % ("=" * 78, "Used by: " + ", ".join(who), "=" * 78, text))
' > "$stage/licenses/rust-crates.txt"

# Sanity: right architecture, glibc <= 2.28, libraries resolvable.
file "$stage/frameplayerx" | grep -q "ARM aarch64" || { echo "not an aarch64 binary" >&2; exit 1; }
max_glibc="$(objdump -T "$stage/frameplayerx" "$stage"/lib/*.so* 2>/dev/null | grep -o 'GLIBC_[0-9.]*' | sort -uV | tail -1)"
echo "highest glibc symbol version: $max_glibc"
case "$max_glibc" in GLIBC_2.2[0-8]|GLIBC_2.1*|GLIBC_2.[0-9]) ;; *) echo "needs newer glibc than 2.28" >&2; exit 1 ;; esac
# shellcheck disable=SC2016 # $ORIGIN is literal in an rpath
readelf -d "$stage/frameplayerx" | grep -qE '(RPATH|RUNPATH).*\$ORIGIN/lib' || { echo "missing \$ORIGIN/lib rpath" >&2; exit 1; }

zip="frameplayerx-$version-aarch64.zip"
rm -f "dist/$zip"
(cd "$root/dist/stage" && find frameplayerx -print0 | sort -z | xargs -0 touch -d "@${SOURCE_DATE_EPOCH:-0}" && zip -qrX9 "$root/dist/$zip" frameplayerx)
echo "== $(du -h "dist/$zip" | cut -f1) dist/$zip"

release="target/release/fp-release"
args=(manifest --version "$version" --zip "dist/$zip" --url-base "$url_base" --out dist/manifest.json)
[ -n "$notes" ] && args+=(--notes "$notes")
"$release" "${args[@]}"
"$release" framedrop --name FramePlayerX --zip "dist/$zip" --url "$url_base/$zip" --out dist/framedrop.json \
  --manifest-url "$url_base/framedrop.json"
# The same file list for Frame Control install links
# (frame-control://install?manifest=<url of frameplayerx.json>).
python3 - dist/framedrop.json > dist/frameplayerx.json <<'P'
import json, sys
fd = json.load(open(sys.argv[1]))
print(json.dumps({"schema": "frame-control.install/v1", "name": fd["name"], "files": fd["files"]}, indent=2))
P
if [ -n "$key" ]; then
  "$release" sign dist/manifest.json --key "$key"
else
  echo "note: dist/manifest.json is unsigned; run: fp-release sign dist/manifest.json --key PREFIX.key"
fi
rm -rf "$root/dist/stage"
ls -la dist
