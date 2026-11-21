#!/usr/bin/env python3
"""Time the benchmark image with the scorer's Linux container limits."""

import argparse
from datetime import datetime
import json
import os
from pathlib import Path
import platform
import shutil
from statistics import median
import subprocess
import sys
import tempfile
from time import perf_counter


PHASES = ("startup", "parse", "full")


def docker(*args):
    return subprocess.check_output(["docker", *args], text=True).strip()


def report_cpu(image, cpuset):
    try:
        cpuinfo = docker(
            "run", "--rm", "--platform", "linux/amd64", "--network", "none",
            "--cpuset-cpus", cpuset, "--entrypoint", "/bin/cat", image, "/proc/cpuinfo",
        )
    except subprocess.CalledProcessError:
        print(f"CPU set {cpuset} | CPU model unavailable", flush=True)
        return
    ids = set()
    for part in cpuset.split(","):
        first, _, last = part.partition("-")
        ids.update(range(int(first), int(last or first) + 1))
    models = set()
    flags = {"aes", "avx2", "bmi2", "avx512f", "avx512bw", "avx512vl", "avx512_vnni", "avx512vbmi2"}
    for block in cpuinfo.split("\n\n"):
        fields = {}
        for line in block.splitlines():
            key, separator, value = line.partition(":")
            if separator:
                fields[key.strip()] = value.strip()
        if int(fields.get("processor", "-1")) in ids:
            models.add(fields.get("model name", "unknown"))
            flags.intersection_update(fields.get("flags", "").split())
    if models:
        names = ", ".join(sorted(models))
        common_flags = " ".join(sorted(flags)) or "none reported"
        print(f"CPU set {cpuset} | {names} | common instruction flags {common_flags}", flush=True)
    else:
        print(f"CPU set {cpuset} | CPU model unavailable", flush=True)


def header_only(source, destination):
    destination.mkdir()
    shutil.copyfile(source / "manifest.json", destination / "manifest.json")
    pairs = [(source / name, destination / name) for name in ("products.csv", "campaigns.csv")]
    for name in ("events", "clicks", "links"):
        (destination / name).mkdir()
        pairs.extend((part, destination / name / part.name) for part in (source / name).glob("*.csv"))
    for original, stripped in pairs:
        with original.open("rb") as stream:
            stripped.write_bytes(stream.readline())


def sample(image, simd, phase, data, directory, args):
    for name in ("output", "scratch"):
        path = directory / name
        path.mkdir(parents=True)
        path.chmod(0o777)

    # Keep the limits and mounts in sync with scripts/docker_run.sh.
    simd_env = f"CSV_SIMD={simd}" if simd else "CSV_SIMD"
    flags = [
        "--platform", "linux/amd64", "--network", "none", "--read-only",
        "--tmpfs", "/tmp:rw,size=64m,mode=1777", "--cap-drop", "ALL",
        "--security-opt", "no-new-privileges", "--pids-limit", "4096",
        "--cpus", str(args.cpus), "--cpuset-cpus", args.cpuset,
        "--memory", args.memory, "--memory-swap", args.memory, "--user", "1000:1000",
        "-v", f"{data}:/data:ro,z", "-v", f"{directory / 'output'}:/output:z",
        "-v", f"{directory / 'scratch'}:/scratch:z",
        "-e", f"NCPU={args.cpus}", "-e", f"MEM_GB={args.memory.removesuffix('g')}",
        "-e", "PROFILE", "-e", simd_env,
    ]
    if phase == "parse":
        flags += ["--entrypoint", "/usr/local/bin/phase-probe"]

    started = perf_counter()
    container = docker("create", *flags, image)
    try:
        result = subprocess.run(["docker", "start", "-a", container], capture_output=True, text=True)
        wall = perf_counter() - started
        state = json.loads(docker("inspect", "--format", "{{json .State}}", container))
        if result.returncode or state["ExitCode"]:
            raise RuntimeError(result.stderr.strip() or state["Error"] or f"{phase} container failed")
        engine_start = datetime.fromisoformat(state["StartedAt"])
        engine_finish = datetime.fromisoformat(state["FinishedAt"])
        return wall, (engine_finish - engine_start).total_seconds(), result.stderr.strip()
    finally:
        docker("rm", "-f", container)


def clear(directory, image):
    try:
        shutil.rmtree(directory)
    except PermissionError:
        # The host may use a different UID from the scorer's container.
        subprocess.run([
            "docker", "run", "--rm", "--platform", "linux/amd64", "--network", "none",
            "--user", "1000:1000", "-v", f"{directory}:/run-data:z", "--entrypoint", "chmod",
            image, "-R", "a+rwX", "/run-data",
        ], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        shutil.rmtree(directory)


def benchmark(args):
    report_cpu(args.image[0], args.cpuset)
    simd_modes = ("auto", "avx2") if args.compare_simd else (None,)
    samples = {
        (image, simd, phase): []
        for image in args.image
        for simd in simd_modes
        for phase in PHASES
    }
    with tempfile.TemporaryDirectory(prefix="shopping-benchmark-") as temp:
        root = Path(temp)
        empty = root / "headers"
        header_only(args.data, empty)
        for round_number in range(args.rounds + 1):
            order = list(samples)
            if round_number % 2 == 0:
                order.reverse()
            for image, simd, phase in order:
                data = empty if phase == "startup" else args.data
                directory = Path(tempfile.mkdtemp(dir=root))
                directory.chmod(0o755)
                try:
                    wall, engine, log = sample(image, simd, phase, data, directory, args)
                finally:
                    clear(directory, image)
                label = f"round {round_number}" if round_number else "warmup"
                build = f"{image}/{simd}" if simd else image
                print(f"{label} {build} {phase} wall={wall:.6f}s engine={engine:.6f}s", flush=True)
                if log:
                    print(log, flush=True)
                if round_number:
                    samples[image, simd, phase].append((wall, engine))
    for (image, simd, phase), timings in samples.items():
        build = f"{image}/{simd}" if simd else image
        wall, engine = map(median, zip(*timings))
        print(f"median {build} {phase} wall={wall:.6f}s engine={engine:.6f}s")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("data", type=Path)
    parser.add_argument("--image", action="append", help="benchmark image tag, repeat for comparisons")
    parser.add_argument(
        "--compare-simd", action="store_true",
        help="alternate automatic and AVX2 scanning in the default x86-64-v3 image",
    )
    parser.add_argument("--rounds", type=int, default=5)
    parser.add_argument("--cpus", type=int, default=8)
    parser.add_argument("--cpuset", default=os.environ.get("SHOPPING_CPUSET"))
    parser.add_argument("--memory", default="24g")
    args = parser.parse_args()
    if platform.system() != "Linux" or platform.machine() != "x86_64":
        parser.error("run on native x86-64 Linux for representative timings")
    if args.rounds < 1 or args.cpus < 1:
        parser.error("--rounds and --cpus must be positive")
    args.image = args.image or ["shopping-sessions:benchmark"]
    if not args.cpuset:
        available = sorted(os.sched_getaffinity(0))
        if len(available) < args.cpus:
            parser.error(f"only {len(available)} CPUs available, pass --cpus or --cpuset")
        args.cpuset = ",".join(map(str, available[:args.cpus]))
    try:
        args.data = args.data.resolve(strict=True)
        if docker("info", "--format", "{{.OSType}} {{.Architecture}}") not in ("linux x86_64", "linux amd64"):
            parser.error("the Docker daemon must also be Linux amd64")
        benchmark(args)
    except (subprocess.CalledProcessError, RuntimeError, OSError) as error:
        print(error, file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
