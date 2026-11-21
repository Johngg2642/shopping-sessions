# Shopping Sessions

Rust pipeline for the Shopping Sessions Buildathon. It reads the mounted CSV dataset and writes `user_daily` and `campaign_daily` as Parquet. The track’s [rules](rules.md) specify the output.

## Run and verify

Build the release binary with Rust 1.100 and install the comparison tool in a Python 3.12 virtual environment.

```sh
cargo build --release --locked
python3.12 -m venv .venv
.venv/bin/python -m pip install -r requirements.txt
make fixtures-native PYTHON=.venv/bin/python
```

Generate the public dataset with the supplied generator and extract the published answer.

```sh
unzip -n generator.zip
unzip -n dev-answer.zip
cargo build --release --locked --manifest-path generator/Cargo.toml
generator/target/release/generate --seed 1 --events 1000000 --clicks 250000 --output dev
PROFILE=1 NCPU=8 target/release/shopping-sessions dev out
make check-dev PYTHON=.venv/bin/python
```

Use a fresh output directory for each run. The scorer mounts an empty one. The binary also accepts `DATA_DIR`, `OUTPUT_DIR`, and `NCPU` through the environment. `PROFILE=1` prints stage wall times on stderr. The scorer’s time includes container startup and shutdown.

```sh
make fixtures PYTHON=.venv/bin/python
PROFILE=1 make run-dev
make check-dev PYTHON=.venv/bin/python
```

Docker builds for `linux/amd64` by default. On Apple silicon, `PLATFORM=linux/arm64` gives faster local Docker iterations. Use native amd64 Linux for representative scorer timings. The amd64 image targets x86-64-v3 with AES-NI for the scorer’s Xeon CPUs. It runs as UID 1000 with the read-only and offline settings in `scripts/docker_run.sh`.

## Pipeline

1. Process newline-aligned chunks of memory-mapped files in parallel. A block scanner finds newlines, commas and quotes together. It uses AVX-512 byte comparisons when available and supports AVX2, NEON and scalar parsing. The amd64 parser uses BMI2 to check quote pairs and extract quote flags for an entire row. Only LF ends a row. Spaces and carriage returns remain in field values.
2. Read each event CSV once while building linked components with union by size and path compression. Each component tracks its byte-wise minimum ID, including IDs absent from events and clicks. Event candidates retain their individual user IDs until the link graph is ready.
3. Deduplicate event IDs in a compact direct index with an exact overflow table. Hash fingerprints avoid loading unrelated rows from occupied buckets. An invalid earliest event copy still suppresses later copies. Route the surviving events and valid clicks by canonical person. Typical event payloads use a window-relative timestamp and packed purchase fields. An overflow representation preserves wider timestamps and categories.
4. Store canonical person IDs as 64-bit keys. Up to nine nonzero ASCII bytes fit inline, and other names use an exact dictionary. Sort each person shard in place and scan its events once to build sessions and daily category totals. Clicks use the same person keys. Clicks at equal timestamps sort by descending click ID so the last eligible click has the smallest ID. Identical copies leave attribution and the merged assist intervals unchanged.
5. Merge overlapping seven-day coverage intervals per campaign. Use binary searches on session starts to count each interval by UTC day. This bounds work when a shared device ties thousands of user IDs together and each campaign has many qualifying clicks.
6. Reuse a Parquet writer and campaign totals across groups of user shards. Reduce the campaign totals and write the campaign-day table. A bounded dense array holds campaign totals for ordinary date windows and a sparse map handles wider ones. Both outputs use Parquet DATE columns and 64-bit counts and revenue.

The `dev/`, `dev-answer/`, and generated benchmark data stay outside Git. The repository includes the 36 small fixtures and their expected results.

## Profile generated inputs

Use a fixed seed and fresh output directories to compare changes. This command generates the 20M-event input used for local profiling.

```sh
bench=$(mktemp -d)
generator/target/release/generate --seed 29 --events 20000000 --clicks 5000000 --output "$bench/data"
PROFILE=1 NCPU=8 time target/release/shopping-sessions "$bench/data" "$bench/out"
```

For a final-size stress test, use seed 47 with 60M events and 15M clicks. Generated stress inputs have no published answers. Run the supplied exact comparisons on the fixtures and public dev input before using the stress inputs for performance measurements.

On native amd64 Linux, choose eight available CPU IDs for `SHOPPING_CPUSET`. Use the same data and CPU set for both runs. The default scanner selects AVX-512 when the CPU supports it. `CSV_SIMD=avx2` forces AVX2 in the amd64 image.

```sh
make build
time env PROFILE=1 SHOPPING_CPUSET=0-7 scripts/docker_run.sh "$bench/data" "$bench/auto"
time env PROFILE=1 SHOPPING_CPUSET=0-7 CSV_SIMD=avx2 scripts/docker_run.sh "$bench/data" "$bench/avx2"
```
