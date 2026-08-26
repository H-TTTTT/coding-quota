from __future__ import annotations

import struct
from pathlib import Path

from PIL import Image, ImageDraw


def _rounded_rect(size: int, radius: int, fill: tuple[int, int, int, int]) -> Image.Image:
    img = Image.new("RGBA", (size, size), (0, 0, 0, 0))
    draw = ImageDraw.Draw(img)
    draw.rounded_rectangle((0, 0, size - 1, size - 1), radius=radius, fill=fill)
    return img


def render(size: int) -> Image.Image:
    pad = max(1, round(size * 0.03))
    tile = size - pad * 2
    radius = max(3, round(tile * 0.22))

    body = _rounded_rect(tile, radius, (28, 32, 38, 255))
    highlight = _rounded_rect(tile, radius, (255, 255, 255, 0))
    hdraw = ImageDraw.Draw(highlight)
    hdraw.rounded_rectangle(
        (1, 1, tile - 2, tile * 0.42),
        radius=max(2, radius - 1),
        fill=(255, 255, 255, 18),
    )
    body = Image.alpha_composite(body, highlight)

    canvas = Image.new("RGBA", (size, size), (0, 0, 0, 0))
    canvas.paste(body, (pad, pad), body)
    draw = ImageDraw.Draw(canvas)

    cx = cy = size / 2
    outer = size * 0.34
    thickness = max(2, int(round(size * (0.16 if size <= 24 else 0.11))))
    bbox = [cx - outer, cy - outer, cx + outer, cy + outer]

    draw.arc(bbox, start=0, end=360, fill=(72, 80, 90, 255), width=thickness)

    start = -90
    end = start + 360 * 0.72
    steps = max(24, size)
    for i in range(steps):
        t = i / (steps - 1)
        a0 = start + (end - start) * t
        a1 = start + (end - start) * min(1.0, t + 1.2 / steps)
        r = int(56 + 40 * t)
        g = int(188 - 20 * t)
        b = int(140 + 10 * t)
        draw.arc(bbox, start=a0, end=a1, fill=(r, g, b, 255), width=thickness)

    if size >= 28:
        bar_w = size * 0.075
        gap = size * 0.035
        heights = (0.34, 0.56, 0.82)
        max_h = size * 0.20
        total_w = 3 * bar_w + 2 * gap
        x0 = cx - total_w / 2
        base_y = cy + size * 0.10
        for i, h in enumerate(heights):
            bh = max_h * h
            x = x0 + i * (bar_w + gap)
            y = base_y - bh
            color = (88 + i * 18, 196, 148, 255)
            draw.rounded_rectangle(
                (x, y, x + bar_w, base_y),
                radius=max(1, bar_w * 0.35),
                fill=color,
            )
    elif size >= 20:
        r = size * 0.09
        draw.ellipse((cx - r, cy - r, cx + r, cy + r), fill=(90, 196, 148, 255))

    return canvas


def make_icon(size: int) -> Image.Image:
    if size <= 16:
        return render(size)
    scale = 4 if size <= 64 else 2
    return render(size * scale).resize((size, size), Image.Resampling.LANCZOS)


def image_to_bmp_ico(image: Image.Image) -> bytes:
    image = image.convert("RGBA")
    width, height = image.size
    xor = bytearray()
    mask = bytearray()
    pixels = image.load()
    for y in range(height - 1, -1, -1):
        for x in range(width):
            red, green, blue, alpha = pixels[x, y]
            xor.extend((blue, green, red, alpha))
        row_bits: list[int] = []
        for x in range(width):
            row_bits.append(1 if pixels[x, y][3] == 0 else 0)
        while len(row_bits) % 32 != 0:
            row_bits.append(0)
        for index in range(0, len(row_bits), 8):
            byte = 0
            for bit in row_bits[index : index + 8]:
                byte = (byte << 1) | bit
            mask.append(byte)
    header = struct.pack(
        "<IIIHHIIIIII",
        40,
        width,
        height * 2,
        1,
        32,
        0,
        len(xor) + len(mask),
        0,
        0,
        0,
        0,
    )
    return header + xor + mask


def write_ico(path: Path, sizes: list[int]) -> None:
    images = [make_icon(size) for size in sizes]
    payloads = [image_to_bmp_ico(image) for image in images]
    offset = 6 + 16 * len(images)
    entries = bytearray()
    data = bytearray()
    for image, payload in zip(images, payloads):
        width, height = image.size
        entries.extend(
            struct.pack(
                "<BBBBHHII",
                0 if width >= 256 else width,
                0 if height >= 256 else height,
                0,
                0,
                1,
                32,
                len(payload),
                offset,
            )
        )
        offset += len(payload)
        data.extend(payload)
    path.write_bytes(struct.pack("<HHH", 0, 1, len(images)) + entries + data)


def main() -> None:
    root = Path(__file__).resolve().parent
    ico = root / "icon.ico"
    write_ico(ico, [16, 24, 32, 48, 64, 128, 256])
    print(f"wrote {ico} ({ico.stat().st_size} bytes)")


if __name__ == "__main__":
    main()
