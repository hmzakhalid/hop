"""Write a Windows .ico holding PNG images (supported since Vista). Usage: ico.py out.ico a.png ..."""
import struct
import sys


def png_size(data):
    assert data[:8] == b"\x89PNG\r\n\x1a\n", "not a PNG"
    return struct.unpack(">II", data[16:24])


out, pngs = sys.argv[1], [open(p, "rb").read() for p in sys.argv[2:]]
offset = 6 + 16 * len(pngs)
header = struct.pack("<HHH", 0, 1, len(pngs))
entries = b""
for data in pngs:
    w, h = png_size(data)
    entries += struct.pack("<BBBBHHII", w % 256, h % 256, 0, 0, 1, 32, len(data), offset)
    offset += len(data)
with open(out, "wb") as f:
    f.write(header + entries + b"".join(pngs))
