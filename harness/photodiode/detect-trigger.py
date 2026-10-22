#!/usr/bin/env python3
"""Find the photodiode triggers in a recording of the rig.

The photodiode, or the LED, sits in the corner of the camera's field of
view. The host drives it in step with the frame it has just captured, so
the frame in which the diode is bright is the frame in which the host
presented that frame.

This script reads the per-frame BGRX files that ``capture.sh`` writes, looks
at a rectangle of the image, and writes out the camera timestamps of the
frames in which the diode was bright. Those timestamps are the ``triggers``
that ``lumen_bench::camera::analyse`` expects.

Usage:
    detect-trigger.py --frames-dir DIR [--roi X Y W H] [--threshold N]
                       [--out FILE]

Exit status:
    0  the triggers were found and written
    1  the frames cannot be read, or no trigger was found
    2  a bad command line
"""

from __future__ import annotations

import argparse
import json
import os
import sys


def parse_args(argv: list[str]) -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description="Find the photodiode triggers in a recording of the rig."
    )
    parser.add_argument("--frames-dir", required=True, help="the directory of the .raw frames")
    parser.add_argument(
        "--roi",
        nargs=4,
        type=int,
        metavar=("X", "Y", "W", "H"),
        default=(0, 0, 64, 64),
        help="the rectangle the diode is in, in pixels (default 0 0 64 64)",
    )
    parser.add_argument(
        "--threshold",
        type=int,
        default=200,
        help="the mean brightness above which the diode counts as lit (default 200)",
    )
    parser.add_argument(
        "--out",
        default="",
        help="where the trigger timestamps go, as JSON (default: stdout)",
    )
    parser.add_argument(
        "--width",
        type=int,
        default=0,
        help="the frame width in pixels; taken from the first frame when it is 0",
    )
    parser.add_argument(
        "--height",
        type=int,
        default=0,
        help="the frame height in pixels; taken from the first frame when it is 0",
    )
    return parser.parse_args(argv)


def frame_files(frames_dir: str) -> list[tuple[int, str]]:
    """The frames of the recording, in camera order."""
    names = []
    for name in os.listdir(frames_dir):
        if not name.endswith(".raw"):
            continue
        stem = name[: -len(".raw")]
        if not stem.isdigit():
            continue
        names.append((int(stem), os.path.join(frames_dir, name)))
    names.sort()
    return names


def mean_luma(path: str, width: int, height: int, roi: tuple[int, int, int, int]) -> float:
    """The mean brightness of the rectangle, in 0..255."""
    x, y, w, h = roi
    if width <= 0 or height <= 0:
        return 0.0
    x = max(0, min(x, width - 1))
    y = max(0, min(y, height - 1))
    w = max(1, min(w, width - x))
    h = max(1, min(h, height - y))
    with open(path, "rb") as handle:
        data = handle.read()
    total = 0.0
    count = 0
    for row in range(y, y + h):
        base = (row * width + x) * 4
        for column in range(x, x + w):
            offset = base + (column - x) * 4
            if offset + 2 >= len(data):
                continue
            blue = data[offset]
            green = data[offset + 1]
            red = data[offset + 2]
            total += 0.114 * blue + 0.587 * green + 0.299 * red
            count += 1
    if count == 0:
        return 0.0
    return total / count


def main(argv: list[str]) -> int:
    args = parse_args(argv)

    if not os.path.isdir(args.frames_dir):
        print(f"detect-trigger: {args.frames_dir} is not a directory", file=sys.stderr)
        return 1

    frames = frame_files(args.frames_dir)
    if not frames:
        print(
            f"detect-trigger: {args.frames_dir} holds no .raw frames with a "
            "numeric name",
            file=sys.stderr,
        )
        return 1

    width = args.width
    height = args.height
    if width <= 0 or height <= 0:
        size = os.path.getsize(frames[0][1])
        # The frames are BGRX, four bytes per pixel, so the aspect is taken
        # from the file size and the caller's height, or the other way round.
        pixels = size // 4
        if args.height > 0:
            width = pixels // args.height
        else:
            height = 720
            width = pixels // height

    levels = []
    for timestamp, path in frames:
        levels.append((timestamp, mean_luma(path, width, height, tuple(args.roi))))

    triggers = [
        timestamp for timestamp, level in levels if level >= args.threshold
    ]
    if not triggers:
        brightest = max((level for _, level in levels), default=0.0)
        print(
            "detect-trigger: the diode never lit up in the rectangle "
            f"{tuple(args.roi)}; the brightest frame was {brightest:.1f}, the "
            f"threshold is {args.threshold}",
            file=sys.stderr,
        )
        return 1

    result = {
        "frames_dir": os.path.abspath(args.frames_dir),
        "frames": len(frames),
        "roi": list(args.roi),
        "threshold": args.threshold,
        "width": width,
        "height": height,
        "triggers_us": triggers,
        "levels": {str(timestamp): round(level, 3) for timestamp, level in levels},
    }

    if args.out:
        with open(args.out, "w", encoding="utf-8") as handle:
            json.dump(result, handle, indent=2, sort_keys=True)
            handle.write("\n")
        print(f"detect-trigger: wrote {args.out} with {len(triggers)} triggers")
    else:
        json.dump(result, sys.stdout, indent=2, sort_keys=True)
        sys.stdout.write("\n")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
