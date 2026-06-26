"""Pre-defined color themes for PPTX and DOCX generation."""

from __future__ import annotations

from typing import Any

# Each theme provides:
#   name            – display name
#   background      – slide / page background color (hex)
#   title_color     – title text color (hex)
#   body_color      – body text color (hex)
#   accent          – accent color for charts / table headers (hex)
#   accent_2..6     – additional accent colors for charts with multiple series (hex)
#   font            – body font family
#   heading_font    – heading font family
#   table_header_bg – table header background (hex)
#   table_header_fg – table header foreground (hex)
#   table_border    – table border color (hex)

THEMES: dict[str, dict[str, Any]] = {
    "default": {
        "name": "Default",
        "background": "#FFFFFF",
        "title_color": "#1A1A2E",
        "body_color": "#333333",
        "accent": "#4A90D9",
        "accent_2": "#7B68EE",
        "accent_3": "#50C878",
        "accent_4": "#FF6B6B",
        "accent_5": "#FFA500",
        "accent_6": "#87CEEB",
        "font": "Microsoft YaHei",
        "heading_font": "Microsoft YaHei",
        "table_header_bg": "#4A90D9",
        "table_header_fg": "#FFFFFF",
        "table_border": "#CCCCCC",
    },
    "ocean": {
        "name": "Ocean",
        "background": "#F0F8FF",
        "title_color": "#003366",
        "body_color": "#1A3A5C",
        "accent": "#0077B6",
        "accent_2": "#00B4D8",
        "accent_3": "#90E0EF",
        "accent_4": "#023E8A",
        "accent_5": "#0096C7",
        "accent_6": "#48CAE4",
        "font": "Microsoft YaHei",
        "heading_font": "Microsoft YaHei",
        "table_header_bg": "#0077B6",
        "table_header_fg": "#FFFFFF",
        "table_border": "#CAF0F8",
    },
    "forest": {
        "name": "Forest",
        "background": "#F5FFF5",
        "title_color": "#1B4332",
        "body_color": "#2D6A4F",
        "accent": "#40916C",
        "accent_2": "#52B788",
        "accent_3": "#95D5B2",
        "accent_4": "#081C15",
        "accent_5": "#74C69D",
        "accent_6": "#B7E4C7",
        "font": "Microsoft YaHei",
        "heading_font": "Microsoft YaHei",
        "table_header_bg": "#40916C",
        "table_header_fg": "#FFFFFF",
        "table_border": "#D8F3DC",
    },
    "sunset": {
        "name": "Sunset",
        "background": "#FFF8F0",
        "title_color": "#7B2D26",
        "body_color": "#9A4D44",
        "accent": "#E07A5F",
        "accent_2": "#F2CC8F",
        "accent_3": "#81B29A",
        "accent_4": "#3D405B",
        "accent_5": "#E07A5F",
        "accent_6": "#F4A261",
        "font": "Microsoft YaHei",
        "heading_font": "Microsoft YaHei",
        "table_header_bg": "#E07A5F",
        "table_header_fg": "#FFFFFF",
        "table_border": "#F4E4D4",
    },
    "corporate": {
        "name": "Corporate",
        "background": "#FFFFFF",
        "title_color": "#1F3864",
        "body_color": "#2B2B2B",
        "accent": "#2F5496",
        "accent_2": "#4472C4",
        "accent_3": "#70AD47",
        "accent_4": "#ED7D31",
        "accent_5": "#A5A5A5",
        "accent_6": "#5B9BD5",
        "font": "Calibri",
        "heading_font": "Calibri Light",
        "table_header_bg": "#2F5496",
        "table_header_fg": "#FFFFFF",
        "table_border": "#D0D0D0",
    },
    "minimal": {
        "name": "Minimal",
        "background": "#FFFFFF",
        "title_color": "#000000",
        "body_color": "#444444",
        "accent": "#666666",
        "accent_2": "#888888",
        "accent_3": "#AAAAAA",
        "accent_4": "#333333",
        "accent_5": "#555555",
        "accent_6": "#CCCCCC",
        "font": "Helvetica",
        "heading_font": "Helvetica",
        "table_header_bg": "#666666",
        "table_header_fg": "#FFFFFF",
        "table_border": "#E0E0E0",
    },
}


def load_theme(name: str | None) -> dict[str, Any]:
    """Return theme dict for *name*, falling back to 'default'."""
    if name is None:
        return THEMES["default"]
    return THEMES.get(name, THEMES["default"])


def hex_to_rgb(hex_color: str) -> tuple[int, int, int]:
    """Convert '#RRGGBB' to (R, G, B) ints."""
    hex_color = hex_color.lstrip("#")
    if len(hex_color) == 3:
        hex_color = "".join(c * 2 for c in hex_color)
    return tuple(int(hex_color[i : i + 2], 16) for i in (0, 2, 4))
