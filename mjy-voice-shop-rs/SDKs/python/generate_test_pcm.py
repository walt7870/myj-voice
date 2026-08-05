#!/usr/bin/env python3
import math
import struct
import sys


def main() -> None:
    if len(sys.argv) != 2:
        print("usage: generate_test_pcm.py <output.pcm>", file=sys.stderr)
        raise SystemExit(2)
    sample_rate = 16000
    duration_s = 1.2
    frequency = 440.0
    amplitude = 0.22
    samples = int(sample_rate * duration_s)
    with open(sys.argv[1], "wb") as file:
        for index in range(samples):
            value = int(math.sin(2 * math.pi * frequency * index / sample_rate) * amplitude * 32767)
            file.write(struct.pack("<h", value))


if __name__ == "__main__":
    main()
