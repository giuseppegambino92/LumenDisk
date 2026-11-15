#!/usr/bin/env bash
# LumenDisk — coverage-guided fuzzing of the filesystem drivers (libFuzzer via cargo-fuzz).
#
#   scripts/fuzz.sh                 60 s per target (ext4, ntfs, fat, exfat)
#   scripts/fuzz.sh 600             10 minutes per target
#   scripts/fuzz.sh 300 ntfs fat    only some targets
#   FUZZ_FORK=4 scripts/fuzz.sh 3600   4 worker processes per target (crashes don't stop the run)
#   FUZZ_PARALLEL=0 scripts/fuzz.sh    targets one after the other (default: all at once)
#   SKIP_REAL_SEEDS=1 ...           don't build extra seeds with the Docker toolbox
#
# Each target mounts the input as a volume image (core/src/fuzzkit.rs: read-only and read-write,
# full walk, writes, Doctor repair). Seeds: valid images from our own mkfs plus, when Docker and
# the lumendisk-toolbox image are available, images made by mkfs.ext4 -d / mkfs.vfat + mcopy /
# mkntfs + ntfscp / mkfs.exfat. Corpora grow in core/fuzz/corpus/<target> (not committed);
# crashes land in core/fuzz/artifacts/<target>. A crash makes the script exit non-zero; add the
# input to core/tests/corrupt_regressions/ (see README there) once fixed.
#
# Needs a nightly toolchain and cargo-fuzz. Nothing is installed automatically: if they are
# missing the script says how to get them and exits 0.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
export PATH="$HOME/.cargo/bin:$PATH"
SECS="${1:-60}"
shift || true
TARGETS=("$@")
[ ${#TARGETS[@]} -gt 0 ] || TARGETS=(ext4 ntfs fat exfat)
FORK="${FUZZ_FORK:-1}"

if ! cargo +nightly --version >/dev/null 2>&1; then
  echo "fuzz: skipped — no nightly Rust toolchain. Install it with:"
  echo "        rustup toolchain install nightly --profile minimal"
  exit 0
fi
if ! cargo +nightly fuzz --version >/dev/null 2>&1; then
  echo "fuzz: skipped — cargo-fuzz is not installed. Install it with:"
  echo "        cargo install cargo-fuzz"
  exit 0
fi

FUZZ="$ROOT/core/fuzz"
SEEDS="$FUZZ/seeds"
rm -rf "$SEEDS"
mkdir -p "$SEEDS"
( cd "$ROOT/core" && cargo run -q --release --example fuzz_seeds -- "$SEEDS" >/dev/null )

if [ -z "${SKIP_REAL_SEEDS:-}" ] && docker image inspect lumendisk-toolbox >/dev/null 2>&1; then
  W="$ROOT/target/fuzz-real-seeds"
  rm -rf "$W"; mkdir -p "$W"
  if docker run --rm --name "lumendisk-fuzzseeds-$$" -v "$W:/w" -w /w lumendisk-toolbox sh -c '
      set -e
      mkdir -p tree/dir/sub tree/many
      seq 1 4000 > tree/dir/numbers.txt; echo hello > tree/hello.txt; seq 1 300 > tree/dir/sub/deep.txt
      for i in $(seq 1 120); do echo "file $i" > "tree/many/file number $i with a long name.txt"; done
      ln -s dir/numbers.txt tree/link
      ln -s "$(seq -s / 1 40)/target" tree/longlink
      mkfs.ext4 -q -F -b 1024 -N 256 -O metadata_csum,64bit,flex_bg -d tree e4.img 1024
      e2fsck -fyD e4.img >/dev/null 2>&1 || true
      mkfs.ext2 -q -F -b 1024 -N 256 -d tree e2.img 768
      mkfs.ext3 -q -F -b 1024 -N 256 -J size=1 -d tree e3.img 4096
      rm tree/link tree/longlink
      truncate -s 600K f12.img && mkfs.vfat -F 12 f12.img >/dev/null && mcopy -s -i f12.img tree/* ::
      truncate -s 2200K f16.img && mkfs.vfat -F 16 -s 1 f16.img >/dev/null && mcopy -s -i f16.img tree/* ::
      truncate -s 34M f32.img && mkfs.vfat -F 32 -s 1 f32.img >/dev/null && mcopy -s -i f32.img tree/* ::
      truncate -s 4M ex.img && mkfs.exfat ex.img >/dev/null
      truncate -s 2M nt.img && mkntfs -F -f -q nt.img >/dev/null 2>&1
      ntfscp -q nt.img tree/hello.txt hello.txt && ntfscp -q nt.img tree/dir/numbers.txt numbers.txt
      rm -rf tree' >/dev/null 2>&1; then
    ( cd "$ROOT/core"
      cargo run -q --release --example fuzz_seeds -- "$SEEDS" --import ext4 "$W/e4.img" "$W/e2.img" "$W/e3.img"
      cargo run -q --release --example fuzz_seeds -- "$SEEDS" --import fat "$W/f12.img" "$W/f16.img" "$W/f32.img"
      cargo run -q --release --example fuzz_seeds -- "$SEEDS" --import exfat "$W/ex.img"
      cargo run -q --release --example fuzz_seeds -- "$SEEDS" --import ntfs "$W/nt.img" ) >/dev/null 2>&1 || true
  fi
  rm -rf "$W"
fi

# Build once (release + debug assertions / overflow checks, AddressSanitizer).
( cd "$ROOT/core" && cargo +nightly fuzz build >/dev/null 2>&1 ) || ( cd "$ROOT/core" && cargo +nightly fuzz build )
FORKARGS=()
if [ "$FORK" -gt 1 ]; then FORKARGS=(-fork="$FORK" -ignore_crashes=1 -ignore_timeouts=1 -ignore_ooms=1); fi

LOGS="$ROOT/target/fuzz-logs"
mkdir -p "$LOGS"
run_target() {
  local t="$1"
  mkdir -p "$FUZZ/corpus/$t" "$SEEDS/$t"
  # Inputs are whole images: let mutants be as large as the largest seed.
  local maxlen
  maxlen=$(find "$SEEDS/$t" -type f -exec wc -c {} + | awk '$2 != "total" && $1 > m {m = $1} END {print m + 0}')
  [ "$maxlen" -gt 4096 ] || maxlen=1048576
  ( cd "$ROOT/core" && cargo +nightly fuzz run "$t" "$FUZZ/corpus/$t" "$SEEDS/$t" -- \
      -max_len="$maxlen" -max_total_time="$SECS" -timeout=10 -rss_limit_mb=2048 -malloc_limit_mb=1024 \
      ${FORKARGS[@]+"${FORKARGS[@]}"} -print_final_stats=1 ) >"$LOGS/$t.log" 2>&1
}

PIDS=()
STATUS=0
for t in "${TARGETS[@]}"; do
  if [ "${FUZZ_PARALLEL:-1}" = 1 ]; then
    run_target "$t" & PIDS+=("$!")
  else
    run_target "$t" || STATUS=1
  fi
done
for p in "${PIDS[@]}"; do wait "$p" || STATUS=1; done

for t in "${TARGETS[@]}"; do
  L="$LOGS/$t.log"
  EXECS=$(grep -Eo 'stat::number_of_executed_units: *[0-9]+' "$L" | awk '{s+=$2} END {print s+0}')
  if [ "$EXECS" = 0 ]; then EXECS=$(grep -Eo '#[0-9]+' "$L" | tail -1 | tr -d '#'); fi
  COV=$(grep -Eo 'cov: [0-9]+' "$L" | tail -1 | awk '{print $2}')
  NCORP=$(ls "$FUZZ/corpus/$t" 2>/dev/null | wc -l | tr -d ' ')
  CRASH=$(ls "$FUZZ/artifacts/$t" 2>/dev/null | { grep -E '^(crash|timeout|oom|leak)-' || true; } | wc -l | tr -d ' ')
  printf '  %-6s %4ss  %10s execs  cov %-6s corpus %-5s crashes %s\n' "$t" "$SECS" "${EXECS:-?}" "${COV:-?}" "$NCORP" "$CRASH"
  if [ "$CRASH" != 0 ]; then
    STATUS=1
    ls "$FUZZ/artifacts/$t" | grep -E '^(crash|timeout|oom|leak)-' | sed "s|^|    $FUZZ/artifacts/$t/|"
    { grep -E 'panicked|ERROR: libFuzzer|SUMMARY' "$L" || true; } | head -5 | sed 's/^/    /'
  fi
done
rm -rf "$SEEDS"
exit $STATUS
