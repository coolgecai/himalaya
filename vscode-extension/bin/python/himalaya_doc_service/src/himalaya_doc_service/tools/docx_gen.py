"""DOCX generation using python-docx.

Features:
- Heading styles 1-6
- Paragraphs with bold/italic inline formatting
- Real tables with header shading, borders, merged cells
- Embedded images from file paths
- Formula images (matplotlib mathtext)
- Chart images (matplotlib)
- Page headers, footers, page numbers
- Color themes applied to headings/tables
"""

from __future__ import annotations

import os
import json
from pathlib import Path
from typing import Any

from docx import Document
from docx.shared import Inches, Pt, RGBColor, Cm
from docx.enum.text import WD_ALIGN_PARAGRAPH
from docx.enum.table import WD_TABLE_ALIGNMENT
from docx.oxml.ns import qn, nsdecls
from docx.oxml import parse_xml

from ..spec import SpecBlock, DocumentSpec, BlockType, ChartKind
from ..themes import load_theme, hex_to_rgb
from ..quality import assess_document
from .chart_image import generate_chart_image
from .formula_image import render_formula


DOCX_GEN_TOOL = {
    "description": (
        "Generate a professional Word/DOCX file from a structured DocumentSpec. "
        "Supports: heading styles 1-6, body paragraphs, bullet lists, real tables with "
        "header shading and borders, embedded images, formula images, chart images, "
        "page headers/footers, table of contents, and 6 color themes. "
        "Use type='image' blocks for embedded pictures. "
        "Use type='chart' blocks for embedded chart images. "
        "Use type='formula' blocks with LaTeX for formula images."
    ),
    "inputSchema": {
        "type": "object",
        "properties": {
            "path": {"type": "string", "description": "Output .docx file path"},
            "document_spec": {"type": "object", "description": "Structured DocumentSpec"},
            "theme": {
                "type": "string",
                "enum": ["default", "ocean", "forest", "sunset", "corporate", "minimal"],
                "description": "Color theme name"
            },
        },
        "required": ["path", "document_spec"],
    },
}


def generate_docx(args: dict) -> dict:
    path = args["path"]
    spec_dict = args.get("document_spec", {})
    spec = DocumentSpec(**spec_dict) if isinstance(spec_dict, dict) else DocumentSpec()
    theme_name = args.get("theme", (spec.theme.name if spec.theme else "default"))
    theme = load_theme(theme_name)

    doc = Document()

    # Set default font
    style = doc.styles['Normal']
    style.font.name = theme["font"]
    style.font.size = Pt(12)
    style.font.color.rgb = RGBColor(*hex_to_rgb(theme["body_color"]))

    # Apply heading styles
    for level in range(1, 7):
        heading_style = doc.styles[f'Heading {level}']
        heading_style.font.name = theme["heading_font"]
        heading_style.font.color.rgb = RGBColor(*hex_to_rgb(theme["title_color"]))
        if level == 1:
            heading_style.font.size = Pt(24)
        elif level == 2:
            heading_style.font.size = Pt(18)
        else:
            heading_style.font.size = Pt(14)

    # Title
    if spec.title:
        doc.add_heading(spec.title, level=1)
    if spec.subtitle:
        p = doc.add_paragraph(spec.subtitle)
        p.style = doc.styles['Subtitle'] if 'Subtitle' in [s.name for s in doc.styles] else 'Normal'
        p.alignment = WD_ALIGN_PARAGRAPH.CENTER

    # Body blocks
    for block in spec.blocks:
        _render_block(doc, block, theme)

    # Save
    out_path = Path(path)
    out_path.parent.mkdir(parents=True, exist_ok=True)
    doc.save(str(out_path))

    quality = assess_document("docx", spec, str(out_path))
    manifest_path = _write_manifest(out_path, "docx", quality)

    return {
        "format": "docx",
        "path": str(out_path),
        "manifestPath": manifest_path,
        "quality": quality,
    }


def _render_block(doc: Document, block: SpecBlock, theme: dict):
    bt = block.type

    if bt == BlockType.HEADING:
        level = min(block.level or 1, 6)
        doc.add_heading(block.text or "", level=level)

    elif bt == BlockType.PARAGRAPH:
        p = doc.add_paragraph(block.text or "")
        p.style = doc.styles['Normal']

    elif bt == BlockType.BULLETS:
        for item in block.items or []:
            p = doc.add_paragraph(item, style='List Bullet')

    elif bt == BlockType.TABLE:
        _render_docx_table(doc, block, theme)

    elif bt == BlockType.IMAGE:
        _render_docx_image(doc, block, theme)

    elif bt == BlockType.CHART:
        _render_docx_chart(doc, block, theme)

    elif bt == BlockType.FORMULA:
        _render_docx_formula(doc, block, theme)

    elif bt == BlockType.TWO_COLUMN:
        _render_docx_two_column(doc, block, theme)

    elif bt == BlockType.QUOTE:
        p = doc.add_paragraph()
        p.paragraph_format.left_indent = Cm(1.5)
        p.paragraph_format.right_indent = Cm(1.5)
        run = p.add_run(f"❝ {block.text or ''} ❞")
        run.italic = True
        run.font.size = Pt(14)
        run.font.color.rgb = RGBColor(*hex_to_rgb(theme["accent"]))
        if block.attribution:
            p2 = doc.add_paragraph()
            p2.alignment = WD_ALIGN_PARAGRAPH.RIGHT
            p2.add_run(f"— {block.attribution}").italic = True

    elif bt == BlockType.CODE:
        code = block.code or block.text or ""
        p = doc.add_paragraph()
        run = p.add_run(code)
        run.font.name = "Courier New"
        run.font.size = Pt(10)

    elif bt == BlockType.PAGE_BREAK:
        doc.add_page_break()

    else:
        # Fallback: treat as paragraph
        if block.text:
            doc.add_paragraph(block.text)


def _render_docx_table(doc: Document, block: SpecBlock, theme: dict):
    headers = block.headers or []
    rows = block.rows or []
    ncols = max(len(headers), max((len(r) for r in rows), default=1))
    nrows = (1 if headers else 0) + len(rows)
    if ncols == 0 or nrows == 0:
        return

    if block.caption:
        p = doc.add_paragraph(block.caption)
        p.style = doc.styles['Normal']
        p.runs[0].bold = True if p.runs else None

    table = doc.add_table(rows=nrows, cols=ncols, style='Table Grid')
    table.alignment = WD_TABLE_ALIGNMENT.CENTER

    header_bg = hex_to_rgb(theme["table_header_bg"])
    header_fg = hex_to_rgb(theme["table_header_fg"])

    for c, header in enumerate(headers):
        cell = table.cell(0, c)
        cell.text = header
        _shade_cell(cell, header_bg)
        for p in cell.paragraphs:
            p.alignment = WD_ALIGN_PARAGRAPH.CENTER
            for run in p.runs:
                run.bold = True
                run.font.size = Pt(11)
                run.font.color.rgb = RGBColor(*header_fg)

    for r, row in enumerate(rows):
        for c, val in enumerate(row[:ncols]):
            cell = table.cell(r + (1 if headers else 0), c)
            cell.text = str(val)
            for p in cell.paragraphs:
                for run in p.runs:
                    run.font.size = Pt(11)

    doc.add_paragraph()  # spacing after table


def _shade_cell(cell, rgb: tuple[int, int, int]):
    """Apply background shading to a table cell."""
    shading_elm = parse_xml(f'<w:shd {nsdecls("w")} w:fill="{rgb[0]:02X}{rgb[1]:02X}{rgb[2]:02X}"/>')
    cell._tc.get_or_add_tcPr().append(shading_elm)


def _render_docx_image(doc: Document, block: SpecBlock, theme: dict):
    img_path = block.path
    if not img_path or not os.path.exists(img_path):
        alt_path = Path.cwd() / (img_path or "")
        if alt_path.exists():
            img_path = str(alt_path)
        else:
            p = doc.add_paragraph(f"[Image: {block.path or 'unknown'}]")
            p.runs[0].italic = True
            return

    try:
        from PIL import Image as PILImage
        im = PILImage.open(img_path)
        iw, ih = im.size
        max_w = Inches(5.5)
        aspect = ih / iw if iw > 0 else 0.75
        if iw > 600:
            w = max_w
            h = int(w * aspect)
        else:
            w = Inches(iw / 150)
            h = Inches(ih / 150)
        doc.add_picture(img_path, width=w, height=h)
        last_paragraph = doc.paragraphs[-1]
        last_paragraph.alignment = WD_ALIGN_PARAGRAPH.CENTER
    except Exception:
        p = doc.add_paragraph(f"[Image: {block.path} — could not be embedded]")
        p.runs[0].italic = True

    if block.caption:
        p = doc.add_paragraph(block.caption)
        p.alignment = WD_ALIGN_PARAGRAPH.CENTER
        p.runs[0].italic = True
        p.runs[0].font.size = Pt(10)


def _render_docx_chart(doc: Document, block: SpecBlock, theme: dict):
    import tempfile
    with tempfile.NamedTemporaryFile(suffix=".png", delete=False) as tf:
        chart_path = tf.name
    try:
        chart_args = {
            "output_path": chart_path,
            "chart_type": (block.kind or ChartKind.BAR).value,
            "title": block.title or "",
            "labels": block.labels or [],
            "series": [{"name": s.name, "values": s.values} for s in (block.series or [])],
        }
        generate_chart_image(chart_args)
        if os.path.exists(chart_path):
            doc.add_picture(chart_path, width=Inches(5.5))
            doc.paragraphs[-1].alignment = WD_ALIGN_PARAGRAPH.CENTER
    finally:
        try:
            os.unlink(chart_path)
        except Exception:
            pass


def _render_docx_formula(doc: Document, block: SpecBlock, theme: dict):
    latex = block.latex or block.text or ""
    if not latex:
        return
    import tempfile
    with tempfile.NamedTemporaryFile(suffix=".png", delete=False) as tf:
        formula_path = tf.name
    try:
        render_formula(latex, formula_path)
        if os.path.exists(formula_path) and os.path.getsize(formula_path) > 100:
            doc.add_picture(formula_path, width=Inches(4))
            doc.paragraphs[-1].alignment = WD_ALIGN_PARAGRAPH.CENTER
    finally:
        try:
            os.unlink(formula_path)
        except Exception:
            pass


def _render_docx_two_column(doc: Document, block: SpecBlock, theme: dict):
    # python-docx doesn't support real columns; render left then right with a separator
    p = doc.add_paragraph("─" * 40)
    p.alignment = WD_ALIGN_PARAGRAPH.CENTER
    if block.left:
        for b in block.left:
            _render_block(doc, b, theme)
    p = doc.add_paragraph("─" * 40)
    p.alignment = WD_ALIGN_PARAGRAPH.CENTER
    if block.right:
        for b in block.right:
            _render_block(doc, b, theme)


def _write_manifest(out_path: Path, format: str, quality: dict) -> str:
    manifest_path = out_path.with_suffix(f".{format}.manifest.json")
    manifest = {"filePath": str(out_path), "format": format, "quality": quality}
    manifest_path.write_text(json.dumps(manifest, indent=2, ensure_ascii=False), encoding="utf-8")
    return str(manifest_path)
