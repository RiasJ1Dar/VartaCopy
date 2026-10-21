"""Generate the original VartaCopy vector and Windows icon assets (Pillow needed)."""
from pathlib import Path
from PIL import Image, ImageDraw

ROOT = Path(__file__).resolve().parents[1] / "assets"
SIZES = (16, 20, 24, 32, 40, 48, 64, 128, 256)


def raster(size):
    scale = size * 4 / 64
    image = Image.new("RGBA", (size * 4, size * 4))
    draw = ImageDraw.Draw(image)
    def rectangle(box, radius, color):
        draw.rounded_rectangle(tuple(round(v * scale) for v in box), round(radius * scale), fill=color)
    rectangle((3, 3, 61, 61), 14, "#155EE8")
    rectangle((13, 12, 39, 43), 5, "#A9DFFF")
    rectangle((16, 15, 36, 40), 2, "#155EE8")
    rectangle((23, 22, 51, 53), 5, "#FFFFFF")
    draw.polygon([(round(x * scale), round(y * scale)) for x, y in
                  [(28, 34), (39, 34), (35, 30), (39, 26), (49, 37), (39, 48), (35, 44), (39, 40), (28, 40)]], fill="#155EE8")
    return image.resize((size, size), Image.Resampling.LANCZOS)


def main():
    ROOT.mkdir(exist_ok=True)
    (ROOT / "vartacopy.svg").write_text('''<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 64 64">
<rect x="3" y="3" width="58" height="58" rx="14" fill="#155EE8"/>
<rect x="13" y="12" width="26" height="31" rx="5" fill="#A9DFFF"/>
<rect x="16" y="15" width="20" height="25" rx="2" fill="#155EE8"/>
<rect x="23" y="22" width="28" height="31" rx="5" fill="white"/>
<path d="M28 34H39L35 30L39 26L49 37L39 48L35 44L39 40H28Z" fill="#155EE8"/>
</svg>
''', encoding="utf-8")
    raster(64).save(ROOT / "vartacopy.png")
    images = [raster(size) for size in SIZES]
    images[-1].save(ROOT / "vartacopy.ico", sizes=[(n, n) for n in SIZES], append_images=images[:-1])


if __name__ == "__main__":
    main()
