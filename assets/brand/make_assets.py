"""Gera os arquivos de imagem do aplicativo a partir da identidade oficial.

    python assets/brand/make_assets.py

Saídas (em apps/opendesk/assets/):
  icon.ico      ícone do executável, de 16 a 256 px
  icon.png      ícone da janela (64 px)
  wordmark.png  o nome "OpenDesk" como máscara em tons de cinza, tingida pelo app

O símbolo é redesenhado em geometria exata (medida no arquivo oficial) em vez
de reduzido do PNG: assim os tamanhos pequenos saem nítidos e com traço mais
grosso, como pede um ícone de 16 px. Requer Pillow.
"""
from pathlib import Path

from PIL import Image, ImageDraw

HERE = Path(__file__).resolve().parent
OUT = HERE.parent.parent / "apps" / "opendesk" / "assets"

NAVY = (1, 33, 86)
BLUE = (1, 118, 251)
TILE = (244, 246, 248)

# O símbolo cabe numa caixa de 824 x 745 unidades.
W, H = 824, 745
STROKE = 80


def mark(size, pad, stroke):
    """Máscaras (monitor da frente, tela de trás) em `size` px; `pad` é a margem, em fração."""
    ss = 8 if size <= 64 else 4
    n = size * ss
    scale = n * (1 - 2 * pad) / W
    ox, oy = (n - W * scale) / 2, (n - H * scale) / 2

    def rounded(draw, x0, y0, x1, y1, r, fill):
        box = [ox + x0 * scale, oy + y0 * scale, ox + x1 * scale, oy + y1 * scale]
        draw.rounded_rectangle(box, radius=r * scale, fill=fill)

    e = (stroke - STROKE) / 2  # traço mais grosso cresce por igual para dentro e para fora
    front = Image.new("L", (n, n), 0)
    d = ImageDraw.Draw(front)
    rounded(d, -e, 196 - e, 652 + e, 648 + e, 105 + e, 255)
    rounded(d, 80 + e, 276 + e, 572 - e, 568 - e, max(40 - e, 8), 0)
    rounded(d, 194, 685 - e, 468, 745 + e, 30 + e, 255)

    back = Image.new("L", (n, n), 0)
    d = ImageDraw.Draw(back)
    rounded(d, 261 - e, -e, 824 + e, 473 + e, 110 + e, 255)
    rounded(d, 341 + e, 80 + e, 744 - e, 393 - e, max(40 - e, 8), 0)
    gap = 34 + e  # a tela de trás para antes de encostar no monitor
    rounded(d, -gap, 196 - gap, 652 + gap, 648 + gap, 105 + gap, 0)

    return front.resize((size, size), Image.LANCZOS), back.resize((size, size), Image.LANCZOS)


def icon(size):
    stroke = {16: 118, 20: 110, 24: 104, 32: 96, 40: 92, 48: 88, 64: 84}.get(size, STROKE)
    pad = 0.12 if size <= 24 else 0.145 if size <= 48 else 0.17
    ss = 8 if size <= 64 else 4
    tile = Image.new("L", (size * ss, size * ss), 0)
    ImageDraw.Draw(tile).rounded_rectangle([0, 0, size * ss - 1, size * ss - 1], radius=size * ss * 0.225, fill=255)
    tile = tile.resize((size, size), Image.LANCZOS)

    clear = Image.new("RGBA", (size, size), (0, 0, 0, 0))
    solid = lambda rgb: Image.new("RGBA", (size, size), rgb + (255,))
    img = Image.composite(solid(TILE), clear, tile)
    front, back = mark(size, pad, stroke)
    img.alpha_composite(Image.composite(solid(BLUE), clear, back))
    img.alpha_composite(Image.composite(solid(NAVY), clear, front))
    return img


def main():
    OUT.mkdir(parents=True, exist_ok=True)
    sizes = [256, 64, 48, 40, 32, 24, 20, 16]
    icons = [icon(s) for s in sizes]
    icons[0].save(OUT / "icon.ico", format="ICO", sizes=[(s, s) for s in sizes], append_images=icons[1:])
    icon(64).save(OUT / "icon.png", optimize=True)

    # O nome: só a cobertura (canal alfa) importa; a cor vem do tema do app.
    alpha = Image.open(HERE / "wordmark.png").convert("RGBA").getchannel("A")
    # Recorta pelo traço de verdade: o arquivo tem ruído quase transparente nas bordas.
    alpha = alpha.crop(alpha.point(lambda a: 255 if a > 96 else 0).getbbox())
    height = 144
    alpha = alpha.resize((round(alpha.width * height / alpha.height), height), Image.LANCZOS)
    alpha.save(OUT / "wordmark.png", optimize=True)
    print("wordmark", alpha.size, "| ícones", sizes)


if __name__ == "__main__":
    main()
