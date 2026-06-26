"""PDF generation using reportlab.

Features:
- CJK font support (automatic detection of Noto Sans CJK)
- Embedded images from file paths
- Real tables with borders and shading
- Formula images (matplotlib mathtext)
- Chart images (matplotlib)
- Page numbers
- A4 page size with configurable margins
"""

from __future__ import annotations

import os
import json
import tempfile
from pathlib import Path
from typing import Any

from reportlab.lib.pagesizes import A4
from reportlab.lib.units import inch, mm
from reportlab.lib.styles import getSampleStyleSheet, ParagraphStyle
from reportlab.lib.enums import TA_LEFT, TA_CENTER, TA_RIGHT, TA_JUSTIFY
from reportlab.lib.colors import HexColor
from reportlab.platypus import (SimpleDocTemplate, Paragraph, Spacer, Table,
                                 TableStyle, Image, PageBreak, KeepTogether)
from reportlab.platypus.flowables import HRFlowable
from reportlab.pdfbase import pdfmetrics
from reportlab.pdfbase.ttfonts import TTFont
from reportlab.pdfbase.cidfonts import UnicodeCIDFont

from ..spec import SpecBlock, DocumentSpec, BlockType, ChartKind
from ..themes import load_theme, hex_to_rgb
from ..quality import assess_document
from .chart_image import generate_chart_image
from .formula_image import render_formula


PDF_GEN_TOOL = {
    "description": (
        "Generate a printable PDF document from a structured DocumentSpec. "
        "Supports: CJK text (Chinese/Japanese/Korean) via automatic font detection, "
        "embedded images, real tables with borders, formula images, chart images, "
        "page numbers, headings, paragraphs, bullet lists, and page breaks."
    ),
    "inputSchema": {
        "type": "object",
        "properties": {
            "path": {"type": "string", "description": "Output .pdf file path"},
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

# ---------------------------------------------------------------------------
# CJK font discovery
# ---------------------------------------------------------------------------

_CJK_FONT_REGISTERED = False
_CJK_FONT_NAME = "Helvetica"


def _register_cjk_font():
    """Discover and register a CJK-capable font."""
    global _CJK_FONT_REGISTERED, _CJK_FONT_NAME
    if _CJK_FONT_REGISTERED:
        return

    search_paths = [
        "/usr/share/fonts",
        "/usr/local/share/fonts",
        os.path.expanduser("~/.fonts"),
        "C:\\Windows\\Fonts",
    ]
    candidates = []

    for base in search_paths:
        if not os.path.exists(base):
            continue
        for root, dirs, files in os.walk(base):
            for f in files:
                fl = f.lower()
                if any(name in fl for name in [
                    'notosanscjk', 'noto sans cjk', 'sourcehansans', 'source han sans',
                    'wqy', 'wenquan', 'simsun', 'simhei', 'songti', 'heiti',
                    'notoserifcjk', 'sourcehanserif',
                ]) and fl.endswith(('.ttf', '.otf', '.ttc')):
                    candidates.append(os.path.join(root, f))
            if len(candidates) > 0:
                break
        if candidates:
            break

    if candidates:
        try:
            pdfmetrics.registerFont(TTFont('CJKFont', candidates[0]))
            _CJK_FONT_NAME = 'CJKFont'
            _CJK_FONT_REGISTERED = True
            return
        except Exception:
            pass

    # Fallback: try UnicodeCIDFont (built-in CJK support in reportlab)
    try:
        pdfmetrics.registerFont(UnicodeCIDFont('STSong-Light'))
        _CJK_FONT_NAME = 'STSong-Light'
        _CJK_FONT_REGISTERED = True
        return
    except Exception:
        pass

    _CJK_FONT_REGISTERED = True


def _cjk_font_name() -> str:
    _register_cjk_font()
    return _CJK_FONT_NAME


# ---------------------------------------------------------------------------
# Generator
# ---------------------------------------------------------------------------

def generate_pdf(args: dict) -> dict:
    path = args["path"]
    spec_dict = args.get("document_spec", {})
    spec = DocumentSpec(**spec_dict) if isinstance(spec_dict, dict) else DocumentSpec()
    theme_name = args.get("theme", (spec.theme.name if spec.theme else "default"))
    theme = load_theme(theme_name)

    _register_cjk_font()
    font_name = _cjk_font_name()

    # Build story. Temp files from chart/formula rendering must outlive doc.build()
    # because reportlab Image flowables load lazily.
    tmp_dir = tempfile.mkdtemp(prefix="himalaya_pdf_")

    try:
        story = []

        # Title
        if spec.title:
            title_style = ParagraphStyle(
                'DocTitle', fontName=font_name, fontSize=24, leading=30,
                textColor=HexColor(theme["title_color"]),
                alignment=TA_CENTER, spaceAfter=12,
            )
            story.append(Paragraph(spec.title, title_style))

        if spec.subtitle:
            sub_style = ParagraphStyle(
                'DocSubtitle', fontName=font_name, fontSize=14, leading=18,
                textColor=HexColor(theme["body_color"]),
                alignment=TA_CENTER, spaceAfter=20,
            )
            story.append(Paragraph(spec.subtitle, sub_style))

        # Blocks
        for block in spec.blocks:
            elements = _render_block(block, theme, font_name, tmp_dir)
            story.extend(elements)

        # Build PDF
        out_path = Path(path)
        out_path.parent.mkdir(parents=True, exist_ok=True)
        doc = SimpleDocTemplate(
            str(out_path), pagesize=A4,
            leftMargin=25*mm, rightMargin=25*mm,
            topMargin=25*mm, bottomMargin=25*mm,
        )
        doc.build(story, onFirstPage=_add_page_number, onLaterPages=_add_page_number)

        quality = assess_document("pdf", spec, str(out_path))
        manifest_path = _write_manifest(out_path, "pdf", quality)

        return {
            "format": "pdf",
            "path": str(out_path),
            "manifestPath": manifest_path,
            "quality": quality,
        }
    finally:
        import shutil
        shutil.rmtree(tmp_dir, ignore_errors=True)


def _add_page_number(canvas, doc):
    """Add page number footer."""
    canvas.saveState()
    canvas.setFont('Helvetica', 9)
    canvas.drawCentredString(A4[0] / 2, 15 * mm, f"Page {doc.page}")
    canvas.restoreState()


def _render_block(block: SpecBlock, theme: dict, font_name: str, tmp_dir: str = "/tmp") -> list:
    """Render a SpecBlock to a list of reportlab flowables."""
    bt = block.type

    if bt == BlockType.HEADING:
        level = block.level or 1
        sizes = {1: 20, 2: 16, 3: 14, 4: 12, 5: 11, 6: 10}
        style = ParagraphStyle(
            f'Heading{level}', fontName=font_name,
            fontSize=sizes.get(level, 14), leading=sizes.get(level, 14) * 1.3,
            textColor=HexColor(theme["title_color"]),
            spaceBefore=12, spaceAfter=6,
        )
        return [Paragraph(_escape_xml(block.text or ""), style)]

    elif bt == BlockType.PARAGRAPH:
        style = ParagraphStyle(
            'Body', fontName=font_name, fontSize=11, leading=16,
            textColor=HexColor(theme["body_color"]),
            spaceBefore=2, spaceAfter=6,
        )
        return [Paragraph(_escape_xml(block.text or ""), style)]

    elif bt == BlockType.BULLETS:
        elements = []
        for item in block.items or []:
            style = ParagraphStyle(
                'Bullet', fontName=font_name, fontSize=11, leading=16,
                textColor=HexColor(theme["body_color"]),
                leftIndent=20, bulletIndent=10, spaceBefore=1, spaceAfter=1,
            )
            elements.append(Paragraph(f"• {_escape_xml(item)}", style))
        return elements

    elif bt == BlockType.TABLE:
        return _render_pdf_table(block, theme, font_name)

    elif bt == BlockType.IMAGE:
        return _render_pdf_image(block, theme)

    elif bt == BlockType.CHART:
        return _render_pdf_chart(block, theme, tmp_dir)

    elif bt == BlockType.FORMULA:
        return _render_pdf_formula(block, theme, tmp_dir)

    elif bt == BlockType.QUOTE:
        style = ParagraphStyle(
            'Quote', fontName=font_name, fontSize=12, leading=18,
            textColor=HexColor(theme["accent"]),
            leftIndent=30, rightIndent=30, spaceBefore=8, spaceAfter=8,
        )
        elements = [Paragraph(f"❝ {_escape_xml(block.text or '')} ❞", style)]
        if block.attribution:
            attr_style = ParagraphStyle(
                'QuoteAttr', fontName=font_name, fontSize=10, leading=14,
                textColor=HexColor(theme["body_color"]),
                alignment=TA_RIGHT, spaceAfter=8,
            )
            elements.append(Paragraph(f"— {_escape_xml(block.attribution)}", attr_style))
        return elements

    elif bt == BlockType.CODE:
        style = ParagraphStyle(
            'Code', fontName='Courier', fontSize=9, leading=12,
            textColor=HexColor(theme["body_color"]),
            backColor=HexColor('#F5F5F5'), leftIndent=10, rightIndent=10,
            spaceBefore=4, spaceAfter=4,
        )
        return [Paragraph(_escape_xml(block.code or block.text or ""), style)]

    elif bt == BlockType.PAGE_BREAK:
        return [PageBreak()]

    elif bt == BlockType.TWO_COLUMN:
        # reportlab columns are complex; render sequentially with separator
        elements = [HRFlowable(width="100%", thickness=0.5, color=HexColor("#CCCCCC"))]
        if block.left:
            for b in block.left:
                elements.extend(_render_block(b, theme, font_name, tmp_dir))
        elements.append(Spacer(1, 10))
        if block.right:
            for b in block.right:
                elements.extend(_render_block(b, theme, font_name, tmp_dir))
        elements.append(HRFlowable(width="100%", thickness=0.5, color=HexColor("#CCCCCC")))
        return elements

    else:
        if block.text:
            style = ParagraphStyle('Fallback', fontName=font_name, fontSize=11, leading=16)
            return [Paragraph(_escape_xml(block.text), style)]
        return []


def _render_pdf_table(block: SpecBlock, theme: dict, font_name: str) -> list:
    headers = block.headers or []
    rows = block.rows or []
    if not headers and not rows:
        return []

    # Build table data
    data = []
    if headers:
        data.append(list(headers))
    for row in rows:
        data.append(list(row[:len(headers)] if headers else row))

    if not data:
        return []

    col_width = 450 / max(len(data[0]), 1) if data else 100
    t = Table(data, colWidths=[col_width] * len(data[0]))

    # Style
    style_cmds = [
        ('GRID', (0, 0), (-1, -1), 0.5, HexColor(theme["table_border"])),
        ('VALIGN', (0, 0), (-1, -1), 'MIDDLE'),
        ('FONTNAME', (0, 0), (-1, -1), font_name),
        ('FONTSIZE', (0, 0), (-1, -1), 9),
    ]
    if headers:
        header_bg = HexColor(theme["table_header_bg"])
        header_fg = HexColor(theme["table_header_fg"])
        style_cmds.extend([
            ('BACKGROUND', (0, 0), (-1, 0), header_bg),
            ('TEXTCOLOR', (0, 0), (-1, 0), header_fg),
            ('FONTNAME', (0, 0), (-1, 0), font_name),
            ('FONTSIZE', (0, 0), (-1, 0), 10),
        ])

    t.setStyle(TableStyle(style_cmds))

    elements = []
    if block.caption:
        cap_style = ParagraphStyle('TblCap', fontName=font_name, fontSize=10, leading=14,
                                   textColor=HexColor(theme["body_color"]), spaceAfter=4)
        elements.append(Paragraph(_escape_xml(block.caption), cap_style))
    elements.append(t)
    elements.append(Spacer(1, 8))
    return elements


def _render_pdf_image(block: SpecBlock, theme: dict) -> list:
    img_path = block.path
    if not img_path or not os.path.exists(img_path):
        alt_path = Path.cwd() / (img_path or "")
        if alt_path.exists():
            img_path = str(alt_path)
        else:
            style = ParagraphStyle('ImgMiss', fontName='Helvetica', fontSize=10,
                                   textColor=HexColor('#999999'))
            return [Paragraph(f"[Image: {block.path or 'unknown'}]", style)]

    try:
        img = Image(img_path, width=400, height=300 if not _get_image_aspect(img_path) else None)
        # Maintain aspect ratio
        from PIL import Image as PILImage
        im = PILImage.open(img_path)
        iw, ih = im.size
        if iw > 0:
            target_w = min(450, iw / 2)
            aspect = ih / iw
            img = Image(img_path, width=target_w, height=target_w * aspect)
        elements = [img]
        if block.caption:
            cap_style = ParagraphStyle('ImgCap', fontName='Helvetica', fontSize=9, leading=12,
                                       textColor=HexColor('#666666'), alignment=TA_CENTER)
            elements.append(Paragraph(_escape_xml(block.caption), cap_style))
        return elements
    except Exception:
        style = ParagraphStyle('ImgErr', fontName='Helvetica', fontSize=10,
                               textColor=HexColor('#999999'))
        return [Paragraph(f"[Image: {block.path} — error embedding]", style)]


def _get_image_aspect(path: str) -> float:
    try:
        from PIL import Image as PILImage
        im = PILImage.open(path)
        return im.height / im.width if im.width > 0 else 0.75
    except Exception:
        return 0.75


def _render_pdf_chart(block: SpecBlock, theme: dict, tmp_dir: str) -> list:
    import uuid
    chart_path = os.path.join(tmp_dir, f"chart_{uuid.uuid4().hex}.png")
    try:
        chart_args = {
            "output_path": chart_path,
            "chart_type": (block.kind or ChartKind.BAR).value,
            "title": block.title or "",
            "labels": block.labels or [],
            "series": [{"name": s.name, "values": s.values} for s in (block.series or [])],
        }
        generate_chart_image(chart_args)
        if os.path.exists(chart_path) and os.path.getsize(chart_path) > 100:
            img = Image(chart_path, width=420, height=280)
            return [img, Spacer(1, 8)]
    except Exception:
        pass
    return [Paragraph(f"[Chart: {block.title or 'Untitled'}]",
                      ParagraphStyle('ChrtErr', fontName='Helvetica', fontSize=10))]


def _render_pdf_formula(block: SpecBlock, theme: dict, tmp_dir: str) -> list:
    latex = block.latex or block.text or ""
    if not latex:
        return []
    import uuid
    formula_path = os.path.join(tmp_dir, f"formula_{uuid.uuid4().hex}.png")
    try:
        render_formula(latex, formula_path)
        if os.path.exists(formula_path) and os.path.getsize(formula_path) > 100:
            img = Image(formula_path, width=300, height=40)
            return [img, Spacer(1, 6)]
    except Exception:
        pass
    return []


def _escape_xml(text: str) -> str:
    """Escape XML special characters for reportlab Paragraph."""
    return (text
            .replace('&', '&amp;')
            .replace('<', '&lt;')
            .replace('>', '&gt;')
            .replace('"', '&quot;'))


def _write_manifest(out_path: Path, format: str, quality: dict) -> str:
    manifest_path = out_path.with_suffix(f".{format}.manifest.json")
    manifest = {"filePath": str(out_path), "format": format, "quality": quality}
    manifest_path.write_text(json.dumps(manifest, indent=2, ensure_ascii=False), encoding="utf-8")
    return str(manifest_path)
