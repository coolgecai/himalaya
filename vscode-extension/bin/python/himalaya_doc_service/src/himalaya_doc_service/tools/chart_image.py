"""Standalone chart image generation using matplotlib.

Generates PNG/SVG chart images that can be embedded in PPTX, DOCX, or PDF
documents, or used independently.
"""

from __future__ import annotations

import os
from pathlib import Path
from typing import Any

import matplotlib
matplotlib.use('Agg')
import matplotlib.pyplot as plt
import matplotlib.ticker as ticker
from matplotlib import font_manager


CHART_IMAGE_TOOL = {
    "description": (
        "Generate a chart as a standalone PNG/SVG image file. "
        "Use this when a chart needs to be embedded in a PPTX or DOCX later, "
        "or when a standalone high-resolution chart image is needed. "
        "Supports: bar, line, pie, scatter, area, radar charts with customizable "
        "colors, sizes, and formats."
    ),
    "inputSchema": {
        "type": "object",
        "properties": {
            "output_path": {"type": "string", "description": "Output image file path"},
            "chart_type": {
                "type": "string",
                "enum": ["bar", "line", "pie", "scatter", "area", "radar"],
                "description": "Chart type"
            },
            "title": {"type": "string", "description": "Chart title"},
            "labels": {"type": "array", "items": {"type": "string"}, "description": "Category labels"},
            "series": {
                "type": "array",
                "items": {
                    "type": "object",
                    "properties": {
                        "name": {"type": "string"},
                        "values": {
                            "type": "array",
                            "items": {"oneOf": [{"type": "number"}, {"type": "string"}]},
                        },
                    },
                },
                "description": "Data series"
            },
            "width": {"type": "integer", "description": "Pixel width (default 1200)"},
            "height": {"type": "integer", "description": "Pixel height (default 800)"},
            "format": {"type": "string", "enum": ["png", "svg"], "default": "png"},
            "color_scheme": {
                "type": "string",
                "enum": ["default", "pastel", "vivid", "monochrome"],
                "default": "default",
            },
        },
        "required": ["output_path", "chart_type", "labels", "series"],
    },
}

# Color schemes
COLOR_SCHEMES = {
    "default": ['#4A90D9', '#FF6B6B', '#50C878', '#FFA500', '#7B68EE', '#87CEEB',
                '#FFB347', '#FF69B4', '#20B2AA', '#DDA0DD'],
    "pastel": ['#A8D8EA', '#FFD3B6', '#D4F0C0', '#FFE5B4', '#C3B1E1', '#B5EAD7',
               '#FFDAB9', '#E8D5B7', '#C1E1C1', '#D8BFD8'],
    "vivid": ['#FF0000', '#00FF00', '#0000FF', '#FFFF00', '#FF00FF', '#00FFFF',
              '#FF4500', '#32CD32', '#1E90FF', '#FF1493'],
    "monochrome": ['#333333', '#666666', '#999999', '#CCCCCC', '#555555', '#777777',
                   '#444444', '#888888', '#AAAAAA', '#BBBBBB'],
}

# Try to find a CJK font for matplotlib
_CJK_FONT = None

def _find_cjk_font():
    global _CJK_FONT
    if _CJK_FONT:
        return _CJK_FONT
    # Search for CJK-capable fonts
    candidates = []
    for f in font_manager.fontManager.ttflist:
        fl = f.name.lower()
        if any(n in fl for n in ['noto sans cjk', 'source han sans', 'wqy', 'simhei',
                                   'wenquan', 'songti', 'heiti', 'droid sans fallback']):
            candidates.append(f.name)
    if candidates:
        _CJK_FONT = candidates[0]
    else:
        _CJK_FONT = 'sans-serif'
    return _CJK_FONT


def generate_chart_image(args: dict) -> dict:
    """MCP tool handler for standalone chart image generation."""
    output_path = args["output_path"]
    chart_type = args["chart_type"]
    title = args.get("title", "")
    labels = args.get("labels", [])
    series_data = args.get("series", [])
    width = args.get("width", 1200)
    height = args.get("height", 800)
    fmt = args.get("format", "png")
    color_scheme = args.get("color_scheme", "default")

    colors = COLOR_SCHEMES.get(color_scheme, COLOR_SCHEMES["default"])
    cjk_font = _find_cjk_font()

    dpi = 150
    fig_w = width / dpi
    fig_h = height / dpi

    fig, ax = plt.subplots(figsize=(fig_w, fig_h))

    n_series = len(series_data)
    x = list(range(len(labels)))
    for series in series_data:
        series["values"] = [_numeric_value(v) for v in series.get("values", [])]

    if chart_type == "bar":
        bar_width = 0.8 / max(n_series, 1)
        for si, series in enumerate(series_data):
            positions = [i + si * bar_width - (n_series - 1) * bar_width / 2 for i in x]
            ax.bar(positions, series.get("values", []), bar_width,
                   label=series.get("name", f"Series {si+1}"),
                   color=colors[si % len(colors)])
        ax.set_xticks(x)
        ax.set_xticklabels(labels, fontproperties=font_manager.FontProperties(fname=None) if cjk_font == 'sans-serif' else None)

    elif chart_type == "line":
        for si, series in enumerate(series_data):
            ax.plot(x, series.get("values", []), marker='o',
                    label=series.get("name", f"Series {si+1}"),
                    color=colors[si % len(colors)], linewidth=2)
        ax.set_xticks(x)
        ax.set_xticklabels(labels)

    elif chart_type == "pie":
        values = series_data[0].get("values", []) if series_data else []
        wedges, texts, autotexts = ax.pie(values, labels=labels, autopct='%1.1f%%',
                                           colors=colors[:len(labels)],
                                           startangle=90)
        for at in autotexts:
            at.set_fontsize(9)

    elif chart_type == "scatter":
        for si, series in enumerate(series_data):
            vals = series.get("values", [])
            ax.scatter(list(range(len(vals))), vals,
                       label=series.get("name", f"Series {si+1}"),
                       color=colors[si % len(colors)], s=80)
        ax.set_xticks(x)
        ax.set_xticklabels(labels)

    elif chart_type == "area":
        for si, series in enumerate(series_data):
            ax.fill_between(x, series.get("values", []), alpha=0.4,
                            color=colors[si % len(colors)],
                            label=series.get("name", f"Series {si+1}"))
            ax.plot(x, series.get("values", []),
                    color=colors[si % len(colors)], linewidth=2)
        ax.set_xticks(x)
        ax.set_xticklabels(labels)

    elif chart_type == "radar":
        import numpy as np
        n_vars = len(labels)
        angles = np.linspace(0, 2 * np.pi, n_vars, endpoint=False).tolist()
        angles += angles[:1]

        ax = fig.add_subplot(111, polar=True)
        for si, series in enumerate(series_data):
            vals = series.get("values", [])
            vals_plot = vals + vals[:1]
            ax.fill(angles, vals_plot, alpha=0.25, color=colors[si % len(colors)])
            ax.plot(angles, vals_plot, 'o-', linewidth=2,
                    label=series.get("name", f"Series {si+1}"),
                    color=colors[si % len(colors)])
        ax.set_xticks(angles[:-1])
        ax.set_xticklabels(labels)

    # Common styling
    if chart_type != "pie" and chart_type != "radar":
        ax.set_title(title, fontsize=16, fontweight='bold', pad=15)
        ax.legend(loc='upper right', framealpha=0.9)
        ax.grid(axis='y', alpha=0.3)
        ax.set_axisbelow(True)
    elif chart_type == "radar":
        ax.set_title(title, fontsize=16, fontweight='bold', pad=20)
        ax.legend(loc='upper right', bbox_to_anchor=(1.3, 1.1))

    fig.tight_layout()

    # Save
    out_path = Path(output_path)
    out_path.parent.mkdir(parents=True, exist_ok=True)
    fig.savefig(str(out_path), dpi=dpi, bbox_inches='tight',
                format=fmt, facecolor='white', edgecolor='none')
    plt.close(fig)

    file_size = os.path.getsize(str(out_path)) if os.path.exists(str(out_path)) else 0
    return {
        "path": str(out_path),
        "format": fmt,
        "size_bytes": file_size,
        "width": width,
        "height": height,
    }


def _numeric_value(value) -> float:
    if isinstance(value, (int, float)):
        return float(value)
    try:
        return float(str(value).replace(",", "").strip())
    except (TypeError, ValueError):
        return 0.0
