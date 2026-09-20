#!/usr/bin/env python3
"""Compile the firmware's portable C codec and check Python CRC/framing vectors."""
import os
import shlex
import pathlib
import random
import struct
import subprocess
import tempfile
import zlib

ROOT = pathlib.Path(__file__).resolve().parents[1]
CODEC = ROOT / "firmware/components/idf_remote"
HARNESS = r'''
#include "mux_codec.h"
#include <stdio.h>
int main(void) {
    mux_decoder_t decoder={0}; mux_frame_t frame; unsigned byte;
    while(scanf("%2x", &byte)==1) {
        if(mux_feed(&decoder,byte,&frame)==1) {
            printf("%u %llu %u ",frame.kind,(unsigned long long)frame.session,frame.request);
            for(size_t i=0;i<frame.size;i++) printf("%02x",frame.data[i]);
            puts("");
            unsigned char wire[MUX_MAX_WIRE]; size_t n=mux_encode(&frame,wire);
            for(size_t i=0;i<n;i++) printf("%02x",wire[i]);
            puts("");
        }
    }
    return mux_crc32((const unsigned char*)"123456789",9)==0xcbf43926 ? 0 : 1;
}
'''
def encode(kind, session, request, payload):
    body = struct.pack("<BBQI", 1, kind, session, request) + payload
    body += struct.pack("<I", zlib.crc32(body))
    return b"\x7e" + b"".join(bytes((0x7d, b ^ 0x20)) if b in (0x7e, 0x7d) else bytes((b,)) for b in body) + b"\x7e"

def main():
    rng = random.Random(42)
    with tempfile.TemporaryDirectory() as temporary:
        directory = pathlib.Path(temporary)
        (directory / "test.c").write_text(HARNESS)
        binary = directory / "codec"
        subprocess.run(shlex.split(os.environ.get("CC", "cc")) + ["-std=c11", "-Wall", "-Wextra", "-Werror", "-I", str(CODEC), str(directory / "test.c"), str(CODEC / "mux_codec.c"), "-o", str(binary)], check=True)
        vectors = [(4, 42, 7, bytes(range(256)))]
        vectors += [(rng.randrange(1, 8), rng.randrange(1, 2**64), rng.randrange(2**32), rng.randbytes(size)) for size in [0, 1, 64, 192, 1024] * 20]
        stream = b"boot log\n"
        expected = []
        for kind, session, request, payload in vectors:
            wire = encode(kind, session, request, payload)
            bad = bytearray(wire); bad[3] ^= 1
            stream += bytes(bad) + b"\x7e\x01" + b"x" * 4000 + b"\x7e" + wire
            expected += [f"{kind} {session} {request} {payload.hex()}".rstrip(), wire.hex()]
        output = subprocess.check_output([str(binary)], input=stream.hex().encode()).decode().splitlines()
        assert [line.rstrip() for line in output] == expected
        print(f"C codec: {len(vectors)} encode/decode vectors, CRC rejection and overflow recovery passed")
if __name__ == "__main__":
    main()
