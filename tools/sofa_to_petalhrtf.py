#!/usr/bin/env python3
"""Convert a SOFA HRTF file to PetalSonic's lightweight .petalhrtf format.

This tool intentionally depends on the HDF5 command-line tool `h5dump` instead
of runtime Python HDF5 bindings. It keeps PetalSonic runtime free from HDF5 and
also works in minimal agent/dev environments where `h5py` is not installed.

Supported input shape for the first version:
- SOFA SimpleFreeFieldHRIR-style datasets
- /SourcePosition shape: (M, 3), Type=spherical, Units=degree,degree,metre
- /Data.IR shape: (M, 2, N)
- /Data.SamplingRate shape: (1)
- /Data.Delay absent or all zero

Coordinate conversion assumes the SOFA listener convention x=front, y=left,
z=up. PetalSonic native HRTF directions are listener-local x=right, y=up,
z=front.
"""

from __future__ import annotations

import argparse
import math
import shutil
import struct
import subprocess
import sys
import tempfile
from pathlib import Path

MAGIC = b"PETHRTF\0"
VERSION = 1


def run_h5dump(args: list[str]) -> subprocess.CompletedProcess[bytes]:
    try:
        return subprocess.run(
            ["h5dump", *args],
            check=True,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
        )
    except FileNotFoundError:
        raise SystemExit("h5dump not found; install HDF5 tools to convert SOFA files")
    except subprocess.CalledProcessError as exc:
        stderr = exc.stderr.decode("utf-8", errors="replace")
        raise SystemExit(f"h5dump failed: {' '.join(args)}\n{stderr}")


def dataset_shape(sofa_path: Path, dataset: str) -> tuple[int, ...]:
    output = run_h5dump(["-H", "-d", dataset, str(sofa_path)]).stdout.decode(
        "utf-8", errors="replace"
    )
    marker = "DATASPACE  SIMPLE { ("
    start = output.find(marker)
    if start < 0:
        raise SystemExit(f"could not find shape for dataset {dataset}")
    start += len(marker)
    end = output.find(")", start)
    if end < 0:
        raise SystemExit(f"could not parse shape for dataset {dataset}")

    dims = tuple(int(part.strip()) for part in output[start:end].split(",") if part.strip())
    if not dims:
        raise SystemExit(f"dataset {dataset} has empty shape")
    return dims


def dump_dataset_le(sofa_path: Path, dataset: str, suffix: str) -> bytes:
    with tempfile.NamedTemporaryFile(suffix=suffix, delete=False) as raw_file:
        raw_path = Path(raw_file.name)
    try:
        # h5dump writes the binary dataset to -o and prints DDL to stdout; ignore stdout.
        run_h5dump(["-d", dataset, "-b", "LE", "-o", str(raw_path), str(sofa_path)])
        return raw_path.read_bytes()
    finally:
        raw_path.unlink(missing_ok=True)


def unpack_f64_array(raw: bytes, expected_count: int, dataset: str) -> list[float]:
    expected_bytes = expected_count * 8
    if len(raw) != expected_bytes:
        raise SystemExit(
            f"dataset {dataset} raw size mismatch: expected {expected_bytes} bytes, got {len(raw)}"
        )
    return [value for (value,) in struct.iter_unpack("<d", raw)]


def read_f64_dataset(sofa_path: Path, dataset: str, shape: tuple[int, ...]) -> list[float]:
    count = math.prod(shape)
    return unpack_f64_array(dump_dataset_le(sofa_path, dataset, ".bin"), count, dataset)


def maybe_read_delay(sofa_path: Path) -> list[float] | None:
    try:
        shape = dataset_shape(sofa_path, "/Data.Delay")
    except SystemExit:
        return None
    return read_f64_dataset(sofa_path, "/Data.Delay", shape)


def sofa_spherical_to_petal_direction(
    azimuth_deg: float,
    elevation_deg: float,
    *,
    azimuth_positive_left: bool,
) -> tuple[float, float, float]:
    azimuth = math.radians(azimuth_deg)
    elevation = math.radians(elevation_deg)

    horizontal = math.cos(elevation)
    front = horizontal * math.cos(azimuth)
    left = horizontal * math.sin(azimuth)
    up = math.sin(elevation)
    right = -left if azimuth_positive_left else left

    length = math.sqrt(right * right + up * up + front * front)
    if length <= 1e-12 or not math.isfinite(length):
        return (0.0, 0.0, 1.0)
    return (right / length, up / length, front / length)


def encode_petalhrtf(
    *,
    sample_rate: int,
    directions: list[tuple[float, float, float]],
    ir: list[float],
    measurement_count: int,
    taps: int,
    receiver_count: int,
    swap_ears: bool,
) -> bytes:
    if receiver_count != 2:
        raise SystemExit(f"expected 2 receivers/ears, got {receiver_count}")
    if sample_rate <= 0:
        raise SystemExit(f"invalid sample rate {sample_rate}")
    if measurement_count != len(directions):
        raise SystemExit(
            f"measurement count mismatch: IR has {measurement_count}, positions have {len(directions)}"
        )
    if taps <= 0:
        raise SystemExit(f"invalid tap count {taps}")

    output = bytearray()
    output += MAGIC
    output += struct.pack("<IIII", VERSION, sample_rate, measurement_count, taps)

    left_receiver = 1 if swap_ears else 0
    right_receiver = 0 if swap_ears else 1

    for measurement_index, direction in enumerate(directions):
        output += struct.pack("<fff", *direction)
        for receiver in (left_receiver, right_receiver):
            base = (measurement_index * receiver_count + receiver) * taps
            for sample in ir[base : base + taps]:
                if not math.isfinite(sample):
                    raise SystemExit(
                        f"non-finite IR sample at measurement {measurement_index}, receiver {receiver}"
                    )
                output += struct.pack("<f", float(sample))

    return bytes(output)


def convert(args: argparse.Namespace) -> None:
    sofa_path = Path(args.input).expanduser().resolve()
    output_path = Path(args.output).expanduser().resolve()

    source_shape = dataset_shape(sofa_path, "/SourcePosition")
    if len(source_shape) != 2 or source_shape[1] != 3:
        raise SystemExit(f"unsupported /SourcePosition shape {source_shape}; expected (M, 3)")

    ir_shape = dataset_shape(sofa_path, "/Data.IR")
    if len(ir_shape) != 3 or ir_shape[1] != 2:
        raise SystemExit(f"unsupported /Data.IR shape {ir_shape}; expected (M, 2, N)")

    measurement_count, receiver_count, taps = ir_shape
    if measurement_count != source_shape[0]:
        raise SystemExit(
            f"measurement count mismatch: SourcePosition has {source_shape[0]}, Data.IR has {measurement_count}"
        )

    if args.max_taps is not None:
        if args.max_taps <= 0:
            raise SystemExit("--max-taps must be positive")
        taps_to_write = min(taps, args.max_taps)
    else:
        taps_to_write = taps

    sr_shape = dataset_shape(sofa_path, "/Data.SamplingRate")
    sample_rate_values = read_f64_dataset(sofa_path, "/Data.SamplingRate", sr_shape)
    if len(sample_rate_values) != 1:
        raise SystemExit("expected one /Data.SamplingRate value")
    sample_rate = int(round(sample_rate_values[0]))

    delay = maybe_read_delay(sofa_path)
    if delay and any(abs(value) > 1e-9 for value in delay) and not args.ignore_delay:
        raise SystemExit(
            "/Data.Delay contains non-zero values; delay baking is not implemented "
            "(pass --ignore-delay to proceed anyway)"
        )

    source_positions = read_f64_dataset(sofa_path, "/SourcePosition", source_shape)
    ir = read_f64_dataset(sofa_path, "/Data.IR", ir_shape)

    if taps_to_write != taps:
        truncated_ir: list[float] = []
        for measurement_index in range(measurement_count):
            for receiver in range(receiver_count):
                base = (measurement_index * receiver_count + receiver) * taps
                truncated_ir.extend(ir[base : base + taps_to_write])
        ir = truncated_ir
        taps = taps_to_write

    directions = []
    for measurement_index in range(measurement_count):
        base = measurement_index * 3
        azimuth, elevation, _distance = source_positions[base : base + 3]
        directions.append(
            sofa_spherical_to_petal_direction(
                azimuth,
                elevation,
                azimuth_positive_left=not args.azimuth_positive_right,
            )
        )

    output = encode_petalhrtf(
        sample_rate=sample_rate,
        directions=directions,
        ir=ir,
        measurement_count=measurement_count,
        taps=taps,
        receiver_count=receiver_count,
        swap_ears=args.swap_ears,
    )

    output_path.parent.mkdir(parents=True, exist_ok=True)
    output_path.write_bytes(output)
    print(
        f"wrote {output_path} ({len(output)} bytes, "
        f"sample_rate={sample_rate}, directions={measurement_count}, taps={taps})"
    )


def main() -> None:
    parser = argparse.ArgumentParser(
        description="Convert SOFA SimpleFreeFieldHRIR data to PetalSonic .petalhrtf"
    )
    parser.add_argument("input", help="input .sofa file")
    parser.add_argument("output", help="output .petalhrtf file")
    parser.add_argument(
        "--max-taps",
        type=int,
        default=None,
        help="truncate each HRIR to at most this many taps",
    )
    parser.add_argument(
        "--swap-ears",
        action="store_true",
        help="swap receiver 0/1 when writing left/right ears",
    )
    parser.add_argument(
        "--azimuth-positive-right",
        action="store_true",
        help="treat positive SOFA azimuth as right instead of the default left",
    )
    parser.add_argument(
        "--ignore-delay",
        action="store_true",
        help="ignore non-zero /Data.Delay values instead of failing",
    )
    args = parser.parse_args()

    if shutil.which("h5dump") is None:
        raise SystemExit("h5dump not found; install HDF5 tools to convert SOFA files")

    convert(args)


if __name__ == "__main__":
    main()
