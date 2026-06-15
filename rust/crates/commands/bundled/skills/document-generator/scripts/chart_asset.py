#!/usr/bin/env python3
"""Render a chart JSON file to a high-resolution PNG or SVG asset."""

from __future__ import annotations

import argparse
import json
from pathlib import Path
from typing import Any


def _numbers(values: list[Any]) -> list[float]:
    result: list[float] = []
    for value in values:
        try:
            result.append(float(value))
        except (TypeError, ValueError):
            result.append(0.0)
    return result


def render_chart(input_path: Path, output_path: Path, width: float, height: float, dpi: int) -> None:
    import matplotlib

    matplotlib.use("Agg")
    import matplotlib.pyplot as plt

    chart = json.loads(input_path.read_text(encoding="utf-8"))
    title = chart.get("title", "")
    kind = chart.get("kind", "bar")
    labels = [str(label) for label in chart.get("labels", [])]
    series = chart.get("series", [])

    if not labels:
        raise SystemExit("chart JSON must include labels")
    if not series:
        raise SystemExit("chart JSON must include at least one series")

    fig, ax = plt.subplots(figsize=(width, height), dpi=dpi)
    x_values = list(range(len(labels)))

    if kind == "line":
        for item in series:
            ax.plot(
                x_values,
                _numbers(item.get("values", [])),
                marker="o",
                linewidth=2.2,
                label=str(item.get("name", "Series")),
            )
    else:
        bar_width = min(0.8 / max(len(series), 1), 0.32)
        offset_start = -bar_width * (len(series) - 1) / 2
        for index, item in enumerate(series):
            offset = offset_start + index * bar_width
            ax.bar(
                [x + offset for x in x_values],
                _numbers(item.get("values", [])),
                width=bar_width,
                label=str(item.get("name", "Series")),
            )

    ax.set_title(title, fontsize=16, fontweight="bold", pad=14)
    ax.set_xticks(x_values)
    ax.set_xticklabels(labels)
    ax.grid(axis="y", color="#D8DEE9", linewidth=0.8)
    ax.spines["top"].set_visible(False)
    ax.spines["right"].set_visible(False)
    ax.legend(frameon=False)
    fig.tight_layout()

    output_path.parent.mkdir(parents=True, exist_ok=True)
    fig.savefig(output_path, bbox_inches="tight")
    plt.close(fig)


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("input", type=Path, help="Chart JSON file")
    parser.add_argument("output", type=Path, help="Output .png or .svg file")
    parser.add_argument("--width", type=float, default=9.0, help="Figure width in inches")
    parser.add_argument("--height", type=float, default=5.0, help="Figure height in inches")
    parser.add_argument("--dpi", type=int, default=220, help="PNG dots per inch")
    args = parser.parse_args()

    render_chart(args.input, args.output, args.width, args.height, args.dpi)


if __name__ == "__main__":
    main()
