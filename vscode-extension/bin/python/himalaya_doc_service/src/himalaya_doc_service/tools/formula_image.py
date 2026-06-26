"""Render LaTeX mathematical formulas as PNG images.

Uses matplotlib's built-in mathtext renderer (no external LaTeX installation required).
Supports a wide subset of LaTeX math notation:
  - Fractions: \\frac{a}{b}
  - Sums/Integrals: \\sum_{i=1}^{n}, \\int_{0}^{\\infty}
  - Greek letters: \\alpha, \\beta, \\gamma, \\pi, \\sigma
  - Subscripts/Superscripts: x_{i}, x^{2}
  - Roots: \\sqrt{x}, \\sqrt[n]{x}
  - Matrices, brackets, and more.
"""

from __future__ import annotations

import os
import re
from pathlib import Path
from typing import Any

import matplotlib
matplotlib.use('Agg')
import matplotlib.pyplot as plt


FORMULA_IMAGE_TOOL = {
    "description": (
        "Render a LaTeX mathematical formula as a high-resolution PNG image. "
        "Uses matplotlib's built-in mathtext renderer — no external LaTeX installation "
        "required. Supports fractions, sums, integrals, Greek letters, subscripts, "
        "superscripts, roots, matrices, and more. The output image can be embedded in "
        "PPTX, DOCX, or PDF documents."
    ),
    "inputSchema": {
        "type": "object",
        "properties": {
            "latex": {"type": "string", "description": "LaTeX formula, e.g. E=mc^2 or \\frac{a}{b}"},
            "output_path": {"type": "string", "description": "Output PNG file path"},
            "font_size": {"type": "integer", "default": 22, "description": "Base font size"},
            "dpi": {"type": "integer", "default": 220, "description": "Output resolution DPI"},
            "transparent_background": {"type": "boolean", "default": True,
                                       "description": "Use transparent background"},
        },
        "required": ["latex", "output_path"],
    },
}


def render_formula(latex: str, output_path: str, font_size: int = 22,
                   dpi: int = 220, transparent: bool = True) -> dict[str, Any]:
    """Render a LaTeX formula string to a PNG image file.

    Args:
        latex: The LaTeX math string (without surrounding $$).
        output_path: Where to write the PNG file.
        font_size: Base font size for the formula.
        dpi: Output resolution.
        transparent: If True, the background will be transparent.
    """
    normalized = _normalize_latex(latex)
    lines = _formula_lines(normalized)
    max_len = max((len(line) for line in lines), default=8)
    fig_w = max(min(max_len * 0.15, 11.0), 2.4)
    fig_h = max(0.55 * len(lines), 0.85)
    out_path = Path(output_path)
    out_path.parent.mkdir(parents=True, exist_ok=True)

    rendered_as = "mathtext"
    try:
        fig, ax = plt.subplots(figsize=(fig_w, fig_h))
        ax.axis('off')
        for idx, line in enumerate(lines):
            expr = line if line.startswith('$') else f'${line}$'
            y = 0.5 if len(lines) == 1 else 1.0 - ((idx + 0.65) / len(lines))
            ax.text(0.5, y, expr, fontsize=font_size,
                    ha='center', va='center',
                    transform=ax.transAxes,
                    color='black')
        fig.savefig(str(out_path), dpi=dpi, bbox_inches='tight',
                    transparent=transparent, pad_inches=0.12,
                    facecolor=None if transparent else 'white')
        plt.close(fig)
    except Exception:
        rendered_as = "plain_latex_image"
        try:
            plt.close('all')
        except Exception:
            pass
        plain = normalized.strip().strip('$')
        fig, ax = plt.subplots(figsize=(fig_w, max(fig_h, 1.0)))
        ax.axis('off')
        ax.text(0.5, 0.5, plain, fontsize=max(font_size - 2, 12),
                ha='center', va='center',
                transform=ax.transAxes,
                color='black',
                family='DejaVu Sans Mono')
        fig.savefig(str(out_path), dpi=dpi, bbox_inches='tight',
                    transparent=transparent, pad_inches=0.15,
                    facecolor=None if transparent else 'white')
        plt.close(fig)

    return {
        "path": str(out_path),
        "latex": latex,
        "normalized": normalized,
        "rendered_as": rendered_as,
        "dpi": dpi,
        "font_size": font_size,
    }


def _normalize_latex(latex: str) -> str:
    expr = (latex or "").strip()
    expr = re.sub(r"^\s*\$\$(.*)\$\$\s*$", r"\1", expr, flags=re.DOTALL)
    expr = re.sub(r"^\s*\$(.*)\$\s*$", r"\1", expr, flags=re.DOTALL)
    expr = re.sub(r"^\\\[(.*)\\\]$", r"\1", expr, flags=re.DOTALL)
    for env in ("equation", "equation*", "align", "align*", "aligned", "gather", "gather*"):
        expr = expr.replace(f"\\begin{{{env}}}", "").replace(f"\\end{{{env}}}", "")
    return " ".join(expr.split()) if "\n" not in expr else expr.strip()


def _formula_lines(expr: str) -> list[str]:
    raw_lines = re.split(r"\\\\|\n", expr)
    lines = [line.strip().strip("&").strip() for line in raw_lines if line.strip()]
    return lines or [expr.strip() or " "]


def handle_formula_image(args: dict) -> dict:
    """MCP tool handler for formula image rendering."""
    latex = args["latex"]
    output_path = args["output_path"]
    font_size = args.get("font_size", 22)
    dpi = args.get("dpi", 220)
    transparent = args.get("transparent_background", True)

    meta = render_formula(latex, output_path, font_size, dpi, transparent)

    file_size = os.path.getsize(output_path) if os.path.exists(output_path) else 0
    return {
        "path": output_path,
        "latex": latex,
        "size_bytes": file_size,
        "font_size": font_size,
        "dpi": dpi,
        "rendered_as": meta.get("rendered_as"),
    }
