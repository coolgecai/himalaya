"""PPTX generation using python-pptx.

Features:
- Real charts (bar, line, pie, scatter, area, radar)
- Embedded images from file paths
- Real tables with formatting
- Formula images (rendered via matplotlib mathtext)
- Multiple slide layouts
- 6 color themes
- Speaker notes support
"""

from __future__ import annotations

import os
import json
import re
import zipfile
from pathlib import Path
from typing import Any

from pptx import Presentation
from pptx.util import Inches, Pt, Emu
from pptx.dml.color import RGBColor
from pptx.enum.text import PP_ALIGN, MSO_ANCHOR, MSO_AUTO_SIZE
from pptx.enum.chart import XL_CHART_TYPE
from pptx.enum.shapes import MSO_SHAPE
from pptx.chart.data import CategoryChartData
from pptx.oxml import parse_xml
from pptx.oxml.ns import qn

from ..spec import SpecBlock, DocumentSpec, BlockType, ChartKind, LayoutBox
from ..themes import load_theme, hex_to_rgb
from ..quality import assess_document
from .formula_image import render_formula
from .office_math import latex_to_powerpoint_math_paragraph


# ---------------------------------------------------------------------------
# Tool descriptor
# ---------------------------------------------------------------------------

PPTX_GEN_TOOL = {
    "description": (
        "Generate a professional PowerPoint/PPTX file from a structured DocumentSpec. "
        "Supports: real charts (bar, line, pie, scatter, area, radar), embedded images from "
        "file paths, editable tables with formatting, editable Office Math formulas from LaTeX "
        "with PNG fallback, multiple slide layouts (title, content, two-column, image+text), "
        "speaker notes, 6 color themes, and CJK font support. Each SpecBlock with type='heading' "
        "and level=1 starts a new slide. "
        "For academic/degree-defense decks, DocumentSpec may include document_type, "
        "source_documents, generation_contract, block source_refs, key_message, layout, "
        "equation_number, and speaker_notes for grounded long-form generation. "
        "Use type='image' blocks with path for embedded images. "
        "Use type='table' blocks for editable tables and type='formula' blocks with latex for "
        "native Office Math equations. Optionally pass asset_manifest_path from "
        "extract_document_assets to hydrate empty image/table/formula placeholders."
    ),
    "inputSchema": {
        "type": "object",
        "properties": {
            "path": {"type": "string", "description": "Output .pptx file path"},
            "document_spec": {"type": "object", "description": "Structured DocumentSpec"},
            "theme": {
                "type": "string",
                "enum": ["default", "ocean", "forest", "sunset", "corporate", "minimal"],
                "description": "Color theme name"
            },
            "slide_width_inches": {"type": "number", "description": "Slide width (default 13.333 for 16:9)"},
            "slide_height_inches": {"type": "number", "description": "Slide height (default 7.5 for 16:9)"},
            "strict_quality": {
                "type": "boolean",
                "default": False,
                "description": "Return an error when the generated quality report contains warnings."
            },
            "asset_manifest_path": {
                "type": "string",
                "description": "Optional assets.manifest.json from extract_document_assets; fills empty image/table/formula blocks."
            },
            "asset_manifest": {
                "type": "object",
                "description": "Optional manifest object returned by extract_document_assets."
            },
        },
        "required": ["path", "document_spec"],
    },
}

# Layout indices in the default template
LAYOUT_TITLE = 0        # Title Slide
LAYOUT_CONTENT = 1      # Title and Content
LAYOUT_TWO_CONTENT = 3  # Two Content
LAYOUT_BLANK = 6        # Blank
LAYOUT_TITLE_ONLY = 5   # Title Only
LAYOUT_SECTION = 2      # Section Header

# Chart type mapping
CHART_TYPE_MAP = {
    ChartKind.BAR: XL_CHART_TYPE.COLUMN_CLUSTERED,
    ChartKind.LINE: XL_CHART_TYPE.LINE_MARKERS,
    ChartKind.PIE: XL_CHART_TYPE.PIE,
    ChartKind.SCATTER: XL_CHART_TYPE.XY_SCATTER,
    ChartKind.AREA: XL_CHART_TYPE.AREA,
    ChartKind.RADAR: XL_CHART_TYPE.RADAR,
}


def _hydrate_spec_with_asset_manifest(spec: DocumentSpec, args: dict) -> tuple[DocumentSpec, dict[str, Any] | None]:
    """Fill empty image/table/formula placeholders from an extracted asset manifest."""

    manifest = _load_asset_manifest(spec, args)
    if not manifest:
        return spec, None

    pools = _asset_manifest_pools(manifest)
    if not any(pools.values()):
        return spec, manifest

    blocks = [_hydrate_block_from_assets(block, pools) for block in spec.blocks]
    return spec.model_copy(update={"blocks": blocks}), manifest


def _load_asset_manifest(spec: DocumentSpec, args: dict) -> dict[str, Any] | None:
    direct = args.get("asset_manifest")
    if isinstance(direct, dict):
        return direct

    metadata = spec.metadata or {}
    candidates = [
        args.get("asset_manifest_path"),
        args.get("assets_manifest_path"),
        metadata.get("asset_manifest_path"),
        metadata.get("assets_manifest_path"),
    ]
    for candidate in candidates:
        if not candidate:
            continue
        path = Path(str(candidate))
        if not path.exists():
            alt = Path.cwd() / str(candidate)
            path = alt if alt.exists() else path
        if not path.exists():
            continue
        try:
            return json.loads(path.read_text(encoding="utf-8"))
        except Exception:
            continue
    return None


def _asset_manifest_pools(manifest: dict[str, Any]) -> dict[str, list[dict[str, Any]]]:
    assets = manifest.get("assets") or []
    image_blocks = list(manifest.get("suggested_image_blocks") or [])
    if not image_blocks:
        image_blocks = [
            asset.get("suggested_block")
            for asset in assets
            if isinstance(asset.get("suggested_block"), dict)
        ]
    table_blocks = list(manifest.get("suggested_table_blocks") or [])
    if not table_blocks:
        table_blocks = [
            asset.get("suggested_table_block")
            for asset in assets
            if isinstance(asset.get("suggested_table_block"), dict)
        ]
    formula_blocks = list(manifest.get("suggested_formula_blocks") or [])
    if not formula_blocks:
        formula_blocks = [
            asset.get("suggested_formula_block")
            for asset in assets
            if isinstance(asset.get("suggested_formula_block"), dict)
        ]
    return {
        "images": [b for b in image_blocks if isinstance(b, dict)],
        "tables": [b for b in table_blocks if isinstance(b, dict)],
        "formulas": [b for b in formula_blocks if isinstance(b, dict)],
    }


def _hydrate_block_from_assets(block: SpecBlock, pools: dict[str, list[dict[str, Any]]]) -> SpecBlock:
    updates: dict[str, Any] = {}
    if block.type == BlockType.IMAGE and not block.path:
        role = ((block.asset_role or "") or str((block.metadata or {}).get("asset_role", ""))).lower()
        replacement = _pop_matching_asset_block(pools["images"], role)
        if replacement:
            updates.update(_empty_field_updates(block, replacement))
    elif block.type == BlockType.TABLE and not (block.headers or block.rows):
        replacement = _pop_matching_asset_block(pools["tables"], "")
        if replacement:
            updates.update(_empty_field_updates(block, replacement))
    elif block.type == BlockType.FORMULA and not (block.latex or block.text):
        replacement = _pop_matching_asset_block(pools["formulas"], "")
        if replacement:
            updates.update(_empty_field_updates(block, replacement))

    if block.left:
        updates["left"] = [_hydrate_block_from_assets(child, pools) for child in block.left]
    if block.right:
        updates["right"] = [_hydrate_block_from_assets(child, pools) for child in block.right]
    return block.model_copy(update=updates) if updates else block


def _pop_matching_asset_block(blocks: list[dict[str, Any]], role: str) -> dict[str, Any] | None:
    if role:
        for idx, block in enumerate(blocks):
            block_role = str(block.get("asset_role") or block.get("assetRole") or "").lower()
            if block_role == role or role in block_role:
                return blocks.pop(idx)
    return blocks.pop(0) if blocks else None


def _empty_field_updates(target: SpecBlock, source_dict: dict[str, Any]) -> dict[str, Any]:
    try:
        source = SpecBlock(**source_dict)
    except Exception:
        return {}
    updates: dict[str, Any] = {}
    for field in SpecBlock.model_fields:
        current = getattr(target, field)
        value = getattr(source, field)
        if _is_empty_value(current) and not _is_empty_value(value):
            updates[field] = value
    return updates


def _is_empty_value(value: Any) -> bool:
    return value is None or value == "" or value == [] or value == {}


def generate_pptx(args: dict) -> dict:
    """Main entry point for PPTX generation from MCP tool call."""
    path = args["path"]
    spec_dict = args.get("document_spec", {})
    spec = DocumentSpec(**spec_dict) if isinstance(spec_dict, dict) else DocumentSpec(**json.loads(spec_dict) if isinstance(spec_dict, str) else {})
    theme_name = args.get("theme", (spec.theme.name if spec.theme else "default"))
    theme = load_theme(theme_name)
    slide_w = args.get("slide_width_inches", 13.333)
    slide_h = args.get("slide_height_inches", 7.5)
    strict_quality = bool(args.get("strict_quality", False))
    spec, asset_manifest = _hydrate_spec_with_asset_manifest(spec, args)

    prs = Presentation()
    prs.slide_width = Inches(slide_w)
    prs.slide_height = Inches(slide_h)

    slides_data = _split_overflow_slides(_split_into_slides(spec.blocks), slide_h)

    for idx, slide_blocks in enumerate(slides_data):
        # Use a blank slide and draw our own stable academic layout. This avoids
        # template placeholder collisions and keeps generated decks predictable.
        slide = prs.slides.add_slide(prs.slide_layouts[LAYOUT_BLANK])
        _paint_slide_background(slide, theme)

        title_text = _find_title(slide_blocks)
        is_title_slide = idx == 0 and not _body_blocks(slide_blocks)
        _render_slide_title(slide, title_text, spec, theme, prs.slide_width, prs.slide_height, is_title_slide)

        _render_blocks(slide, slide_blocks, theme, prs.slide_width, prs.slide_height)
        _apply_speaker_notes(slide, _collect_speaker_notes(slide_blocks))

    # Save
    out_path = Path(path)
    out_path.parent.mkdir(parents=True, exist_ok=True)
    prs.save(str(out_path))

    # Quality
    pptx_structure = _inspect_pptx_structure(out_path)
    quality = assess_document(
        "pptx",
        spec,
        str(out_path),
        slide_count=len(prs.slides),
        asset_manifest=asset_manifest,
        **pptx_structure,
    )
    _merge_layout_quality(quality, _inspect_presentation_layout(prs))
    manifest_path = _write_manifest(out_path, "pptx", quality, spec, asset_manifest)

    if strict_quality and (
        quality.get("warning_count", 0) > 0
        or quality.get("failure_count", 0) > 0
        or quality.get("blocker_count", 0) > 0
    ):
        return {
            "format": "pptx",
            "path": str(out_path),
            "manifestPath": manifest_path,
            "quality": quality,
            "slide_count": len(prs.slides),
            "error": "strict_quality enabled and quality issues were produced",
        }

    return {
        "format": "pptx",
        "path": str(out_path),
        "manifestPath": manifest_path,
        "quality": quality,
        "slide_count": len(prs.slides),
    }


# ---------------------------------------------------------------------------
# Slide splitting
# ---------------------------------------------------------------------------

def _split_into_slides(blocks: list[SpecBlock]) -> list[list[SpecBlock]]:
    """Split blocks into slides. Each heading level=1 starts a new slide."""
    slides: list[list[SpecBlock]] = []
    current: list[SpecBlock] = []

    for block in blocks:
        if block.type == BlockType.PAGE_BREAK:
            if current:
                slides.append(current)
                current = []
            continue
        if block.type == BlockType.HEADING and (block.level or 1) == 1:
            if current:
                slides.append(current)
            current = [block]
        else:
            current.append(block)

    if current or not slides:
        slides.append(current)

    return slides


def _split_overflow_slides(slides: list[list[SpecBlock]], slide_height_inches: float) -> list[list[SpecBlock]]:
    """Split oversized logical slides into continuation slides before rendering."""
    max_body_inches = max(slide_height_inches - 2.25, 3.5)
    out: list[list[SpecBlock]] = []

    for slide_blocks in slides:
        title = _find_title_block(slide_blocks)
        body = [b for b in slide_blocks if b is not title]
        current: list[SpecBlock] = [title] if title else []
        used = 0.0

        for block in body:
            parts = _split_block_for_budget(block, max_body_inches)
            for part in parts:
                height = _estimate_block_height_inches(part)
                has_body = any(not (b.type == BlockType.HEADING and (b.level or 1) == 1) for b in current)
                if has_body and used + height > max_body_inches:
                    out.append(current)
                    current = [_continued_title(title)] if title else []
                    used = 0.0
                current.append(part)
                used += height

        if current:
            out.append(current)

    return out or [[]]


def _find_title_block(blocks: list[SpecBlock]) -> SpecBlock | None:
    for b in blocks:
        if b.type == BlockType.HEADING and (b.level or 1) == 1:
            return b
    return None


def _continued_title(title: SpecBlock | None) -> SpecBlock:
    if title is None:
        return SpecBlock(type=BlockType.HEADING, level=1, text="Continued")
    return title.model_copy(update={"text": f"{title.text or 'Continued'} (continued)"})


def _split_block_for_budget(block: SpecBlock, budget_inches: float) -> list[SpecBlock]:
    if block.type == BlockType.BULLETS and block.items:
        max_items = max(3, int((budget_inches - 0.2) / 0.35))
        if len(block.items) > max_items:
            return [
                block.model_copy(update={"items": block.items[i:i + max_items]})
                for i in range(0, len(block.items), max_items)
            ]
    if block.type == BlockType.TABLE and block.rows:
        header_rows = 1 if block.headers else 0
        max_rows = max(2, int((budget_inches - 0.25) / 0.35) - header_rows)
        if len(block.rows) > max_rows:
            return [
                block.model_copy(update={"rows": block.rows[i:i + max_rows]})
                for i in range(0, len(block.rows), max_rows)
            ]
    if block.type == BlockType.CODE and (block.code or block.text):
        lines = (block.code or block.text or "").splitlines()
        max_lines = max(4, int((budget_inches - 0.2) / 0.30))
        if len(lines) > max_lines:
            return [
                block.model_copy(update={"code": "\n".join(lines[i:i + max_lines]), "text": None})
                for i in range(0, len(lines), max_lines)
            ]
    return [block]


def _estimate_block_height_inches(block: SpecBlock) -> float:
    if block.type == BlockType.HEADING:
        return 0.55
    if block.type == BlockType.PARAGRAPH:
        text_len = len(block.text or "")
        return max(0.45, 0.35 * ((text_len // 95) + 1))
    if block.type == BlockType.BULLETS:
        return 0.35 * max(len(block.items or []), 1) + 0.1
    if block.type == BlockType.TABLE:
        return 0.35 * ((1 if block.headers else 0) + len(block.rows or [])) + 0.2
    if block.type == BlockType.CHART:
        return 4.2
    if block.type == BlockType.IMAGE:
        return 3.4
    if block.type == BlockType.FORMULA:
        return 1.1
    if block.type == BlockType.TWO_COLUMN:
        left = sum(_estimate_block_height_inches(b) for b in (block.left or []))
        right = sum(_estimate_block_height_inches(b) for b in (block.right or []))
        return max(left, right, 0.5)
    if block.type == BlockType.QUOTE:
        return 0.9
    if block.type == BlockType.CODE:
        return 0.30 * max(len((block.code or block.text or "").splitlines()), 1) + 0.2
    return 0.4


def _find_title(blocks: list[SpecBlock]) -> str | None:
    """Return the text of the first heading level=1 block, if any."""
    for b in blocks:
        if b.type == BlockType.HEADING and (b.level or 1) == 1:
            return b.text
    return None


def _body_blocks(blocks: list[SpecBlock]) -> list[SpecBlock]:
    return [b for b in blocks if not (b.type == BlockType.HEADING and (b.level or 1) == 1)]


def _find_key_message(blocks: list[SpecBlock]) -> str | None:
    for b in blocks:
        if b.key_message:
            return b.key_message
    return None


def _collect_speaker_notes(blocks: list[SpecBlock]) -> str:
    notes = []
    for b in blocks:
        if b.speaker_notes:
            notes.append(b.speaker_notes)
    return "\n\n".join(notes)


# ---------------------------------------------------------------------------
# Layout selection
# ---------------------------------------------------------------------------

def _select_layout(prs: Presentation, blocks: list[SpecBlock]) -> int:
    """Select the best slide layout based on block types present."""
    has_title = any(b.type == BlockType.HEADING and (b.level or 1) == 1 for b in blocks)
    has_image = any(b.type == BlockType.IMAGE for b in blocks)
    has_text = any(b.type in (BlockType.PARAGRAPH, BlockType.BULLETS, BlockType.TABLE) for b in blocks)
    has_two_col = any(b.type == BlockType.TWO_COLUMN for b in blocks)

    if has_two_col:
        return LAYOUT_TWO_CONTENT if LAYOUT_TWO_CONTENT < len(prs.slide_layouts) else LAYOUT_CONTENT
    if has_image and has_text:
        return LAYOUT_TWO_CONTENT if LAYOUT_TWO_CONTENT < len(prs.slide_layouts) else LAYOUT_CONTENT
    if has_title and not has_text and not has_image:
        return LAYOUT_TITLE
    if has_title:
        return LAYOUT_CONTENT
    return LAYOUT_BLANK


# ---------------------------------------------------------------------------
# Title styling
# ---------------------------------------------------------------------------

def _apply_title_style(title_shape, text: str, theme: dict):
    """Apply theme-based styling to title placeholder."""
    title_shape.text = ""
    tf = title_shape.text_frame
    tf.clear()
    p = tf.paragraphs[0]
    p.text = text
    p.font.size = Pt(36)
    p.font.bold = True
    p.font.color.rgb = RGBColor(*hex_to_rgb(theme["title_color"]))
    try:
        p.font.name = theme["heading_font"]
    except Exception:
        pass


def _paint_slide_background(slide, theme: dict):
    fill = slide.background.fill
    fill.solid()
    fill.fore_color.rgb = RGBColor(*hex_to_rgb(theme["background"]))


def _render_slide_title(slide, title_text: str | None, spec: DocumentSpec, theme: dict, sw, sh, is_title_slide: bool):
    if not title_text and not is_title_slide:
        return

    if is_title_slide:
        title = title_text or spec.title or "Presentation"
        title_box = slide.shapes.add_textbox(Inches(0.85), Inches(2.25), sw - Inches(1.7), Inches(1.0))
        tf = title_box.text_frame
        tf.clear()
        p = tf.paragraphs[0]
        p.text = title
        p.font.size = Pt(40)
        p.font.bold = True
        p.font.color.rgb = RGBColor(*hex_to_rgb(theme["title_color"]))
        p.alignment = PP_ALIGN.CENTER
        _set_font_name(p, theme["heading_font"])

        subtitle = spec.subtitle or spec.audience or ""
        if subtitle:
            subtitle_box = slide.shapes.add_textbox(Inches(1.6), Inches(3.25), sw - Inches(3.2), Inches(0.55))
            stf = subtitle_box.text_frame
            stf.clear()
            sp = stf.paragraphs[0]
            sp.text = subtitle
            sp.font.size = Pt(19)
            sp.font.color.rgb = RGBColor(*hex_to_rgb(theme["body_color"]))
            sp.alignment = PP_ALIGN.CENTER
            _set_font_name(sp, theme["font"])

        meta = "  |  ".join(part for part in (spec.author, spec.language) if part)
        if meta:
            meta_box = slide.shapes.add_textbox(Inches(1.6), Inches(4.1), sw - Inches(3.2), Inches(0.4))
            mtf = meta_box.text_frame
            mtf.clear()
            mp = mtf.paragraphs[0]
            mp.text = meta
            mp.font.size = Pt(13)
            mp.font.color.rgb = RGBColor(*hex_to_rgb(theme["body_color"]))
            mp.alignment = PP_ALIGN.CENTER
        _add_accent_rule(slide, theme, Inches(3.8), Inches(4.65), sw - Inches(7.6), Inches(0.06))
        return

    title_box = slide.shapes.add_textbox(Inches(0.72), Inches(0.36), sw - Inches(1.44), Inches(0.65))
    tf = title_box.text_frame
    tf.clear()
    p = tf.paragraphs[0]
    p.text = title_text or ""
    p.font.size = Pt(29)
    p.font.bold = True
    p.font.color.rgb = RGBColor(*hex_to_rgb(theme["title_color"]))
    _set_font_name(p, theme["heading_font"])
    _add_accent_rule(slide, theme, Inches(0.75), Inches(1.14), sw - Inches(1.5), Inches(0.04))


def _render_key_message(slide, text: str, theme: dict, x, y, max_w) -> int:
    h = Inches(0.48)
    box = slide.shapes.add_shape(MSO_SHAPE.ROUNDED_RECTANGLE, x, y, max_w, h)
    box.fill.solid()
    box.fill.fore_color.rgb = RGBColor(*hex_to_rgb(theme["accent"]))
    box.line.fill.background()
    tf = box.text_frame
    tf.clear()
    tf.margin_left = Inches(0.18)
    tf.margin_right = Inches(0.18)
    tf.vertical_anchor = MSO_ANCHOR.MIDDLE
    p = tf.paragraphs[0]
    p.text = text
    p.font.size = Pt(15)
    p.font.bold = True
    p.font.color.rgb = RGBColor(255, 255, 255)
    _set_font_name(p, theme["font"])
    return h + Inches(0.14)


def _add_accent_rule(slide, theme: dict, x, y, w, h):
    rule = slide.shapes.add_shape(MSO_SHAPE.RECTANGLE, x, y, w, h)
    rule.fill.solid()
    rule.fill.fore_color.rgb = RGBColor(*hex_to_rgb(theme["accent"]))
    rule.line.fill.background()


def _set_font_name(paragraph, font_name: str):
    try:
        paragraph.font.name = font_name
    except Exception:
        pass


def _apply_speaker_notes(slide, notes: str):
    if not notes:
        return
    try:
        notes_tf = slide.notes_slide.notes_text_frame
        notes_tf.clear()
        notes_tf.text = notes
    except Exception:
        pass


# ---------------------------------------------------------------------------
# Block rendering
# ---------------------------------------------------------------------------

def _render_blocks(slide, blocks: list[SpecBlock], theme: dict, sw, sh):
    """Render SpecBlocks onto a single slide."""
    # Skip the title block (already rendered via placeholder)
    content_blocks = _body_blocks(blocks)

    y = Inches(1.55)  # Start below title area
    x = Inches(1.0)
    max_w = sw - Inches(2.0)

    key_message = _find_key_message(blocks)
    if key_message:
        y += _render_key_message(slide, key_message, theme, x, y, max_w)

    for block in content_blocks:
        height = _render_block(slide, block, theme, x, y, max_w, sw, sh)
        y += height

    # Add remaining text to body placeholder if it exists
    # (already handled by direct shapes)


def _render_block(slide, block: SpecBlock, theme: dict, x, y, max_w, sw, sh) -> int:
    """Render one SpecBlock. Returns height consumed (Emu)."""
    bt = block.type

    if block.position:
        fixed_x, fixed_y, fixed_w, fixed_h = _resolve_position(block.position, sw, sh)
        consumed = _render_block(slide, block.model_copy(update={"position": None}), theme, fixed_x, fixed_y, fixed_w, sw, sh)
        return fixed_h if fixed_h else consumed

    if bt == BlockType.HEADING:
        return _render_heading(slide, block, theme, x, y, max_w)
    elif bt == BlockType.PARAGRAPH:
        return _render_paragraph(slide, block, theme, x, y, max_w)
    elif bt == BlockType.BULLETS:
        return _render_bullets(slide, block, theme, x, y, max_w)
    elif bt == BlockType.TABLE:
        return _render_table(slide, block, theme, x, y, max_w)
    elif bt == BlockType.CHART:
        return _render_chart(slide, block, theme, x, y, max_w)
    elif bt == BlockType.IMAGE:
        return _render_image(slide, block, x, y, max_w, sw, sh)
    elif bt == BlockType.FORMULA:
        return _render_formula_block(slide, block, theme, x, y, max_w)
    elif bt == BlockType.TWO_COLUMN:
        return _render_two_column(slide, block, theme, x, y, max_w, sw, sh)
    elif bt == BlockType.QUOTE:
        return _render_quote(slide, block, theme, x, y, max_w)
    elif bt == BlockType.CODE:
        return _render_code_block(slide, block, theme, x, y, max_w)
    else:
        return 0


def _make_textbox(slide, x, y, w, h):
    """Add a text box shape and return its text frame."""
    txBox = slide.shapes.add_textbox(x, y, w, h)
    txBox.text_frame.word_wrap = True
    txBox.text_frame.auto_size = MSO_AUTO_SIZE.TEXT_TO_FIT_SHAPE
    return txBox.text_frame


def _resolve_position(position: LayoutBox, sw, sh) -> tuple[int, int, int, int]:
    units = (position.units or "fraction").lower()
    x = position.x or 0
    y = position.y or 0
    w = position.w or 0
    h = position.h or 0
    if units == "inches":
        return Inches(x), Inches(y), Inches(w), Inches(h)
    if units == "px":
        # Interpret px against a 1920x1080 design canvas.
        return int(sw * x / 1920), int(sh * y / 1080), int(sw * w / 1920), int(sh * h / 1080)
    return int(sw * x), int(sh * y), int(sw * w), int(sh * h)


# ---------------------------------------------------------------------------
# Individual block renderers
# ---------------------------------------------------------------------------

def _render_heading(slide, block: SpecBlock, theme, x, y, max_w) -> int:
    level = block.level or 2
    sizes = {2: 24, 3: 20, 4: 18, 5: 16, 6: 14}
    sz = Pt(sizes.get(level, 18))
    h = Inches(0.6)
    tf = _make_textbox(slide, x, y, max_w, h)
    p = tf.paragraphs[0]
    p.text = block.text or ""
    p.font.size = sz
    p.font.bold = True
    p.font.color.rgb = RGBColor(*hex_to_rgb(theme["title_color"]))
    _set_font_name(p, theme["heading_font"])
    return Inches(0.5)


def _render_paragraph(slide, block: SpecBlock, theme, x, y, max_w) -> int:
    text = block.text or ""
    h = Inches(max(0.42, 0.32 * ((len(text) // 90) + 1)))
    tf = _make_textbox(slide, x, y, max_w, h)
    p = tf.paragraphs[0]
    p.text = text
    p.font.size = Pt(18)
    p.font.color.rgb = RGBColor(*hex_to_rgb(theme["body_color"]))
    _set_font_name(p, theme["font"])
    return h + Inches(0.08)


def _render_bullets(slide, block: SpecBlock, theme, x, y, max_w) -> int:
    h = Inches(0.4 * max(len(block.items or []), 1))
    tf = _make_textbox(slide, x, y, max_w, h)
    for i, item in enumerate(block.items or []):
        if i == 0:
            p = tf.paragraphs[0]
        else:
            p = tf.add_paragraph()
        p.text = item
        p.font.size = Pt(18)
        p.font.color.rgb = RGBColor(*hex_to_rgb(theme["body_color"]))
        _set_font_name(p, theme["font"])
        p.level = 0
        # bullet character
        pPr = p._pPr
        if pPr is None:
            pPr = p._p.get_or_add_pPr()
        buChar = pPr.makeelement(qn('a:buChar'), {'char': '•'})
        # remove existing buChar if any
        for el in list(pPr):
            if el.tag == qn('a:buChar'):
                pPr.remove(el)
        pPr.append(buChar)
    return Inches(0.35 * max(len(block.items or []), 1))


def _render_table(slide, block: SpecBlock, theme, x, y, max_w) -> int:
    headers = block.headers or []
    rows = block.rows or []
    ncols = max(len(headers), max((len(r) for r in rows), default=1))
    nrows = (1 if headers else 0) + len(rows)
    if ncols == 0 or nrows == 0:
        return 0

    row_h = Inches(0.35)
    tbl_h = row_h * nrows + Inches(0.1)
    tbl_shape = slide.shapes.add_table(nrows, ncols, x, y, max_w, tbl_h)
    tbl_shape.name = "Spec Table"
    tbl = tbl_shape.table

    # Set column widths evenly
    col_w = int(max_w / ncols)
    for c in range(ncols):
        tbl.columns[c].width = col_w

    header_bg = RGBColor(*hex_to_rgb(theme["table_header_bg"]))
    header_fg = RGBColor(*hex_to_rgb(theme["table_header_fg"]))

    for c, header in enumerate(headers):
        cell = tbl.cell(0, c)
        cell.text = header
        _set_cell_fill(cell, header_bg)
        for p in cell.text_frame.paragraphs:
            p.font.size = Pt(14)
            p.font.bold = True
            p.font.color.rgb = header_fg
            p.alignment = PP_ALIGN.CENTER
            _set_font_name(p, theme["font"])

    for r, row in enumerate(rows):
        for c, val in enumerate(row[:ncols]):
            cell = tbl.cell(r + (1 if headers else 0), c)
            cell.text = str(val)
            for p in cell.text_frame.paragraphs:
                p.font.size = Pt(14)
                p.font.color.rgb = RGBColor(*hex_to_rgb(theme["body_color"]))
                _set_font_name(p, theme["font"])

    return tbl_h + Inches(0.15)


def _set_cell_fill(cell, color: RGBColor):
    """Set solid fill on a table cell."""
    tcPr = cell._tc.get_or_add_tcPr()
    solidFill = tcPr.makeelement(qn('a:solidFill'), {})
    srgbClr = solidFill.makeelement(qn('a:srgbClr'), {'val': str(color)})
    solidFill.append(srgbClr)
    # Remove existing fills
    for el in list(tcPr):
        if el.tag in (qn('a:solidFill'), qn('a:noFill')):
            tcPr.remove(el)
    tcPr.append(solidFill)


def _render_chart(slide, block: SpecBlock, theme, x, y, max_w) -> int:
    chart_w = max_w
    chart_h = Inches(4.0)

    chart_type = CHART_TYPE_MAP.get(block.kind or ChartKind.BAR, XL_CHART_TYPE.COLUMN_CLUSTERED)
    chart_data = CategoryChartData()
    chart_data.categories = block.labels or []

    accent_colors = [theme.get(f"accent_{i}", theme["accent"]) for i in range(1, 7)]
    for si, series in enumerate(block.series or []):
        chart_data.add_series(series.name, [_numeric_chart_value(v) for v in series.values])

    chart_frame = slide.shapes.add_chart(chart_type, x, y, chart_w, chart_h, chart_data)
    chart_frame.name = "Spec Chart"
    chart = chart_frame.chart

    if block.title:
        chart.has_title = True
        chart.chart_title.text_frame.paragraphs[0].text = block.title
        chart.chart_title.text_frame.paragraphs[0].font.size = Pt(16)

    # Style the chart with theme colors
    try:
        plot = chart.plots[0]
        for si, series in enumerate(plot.series):
            color = accent_colors[si % len(accent_colors)]
            series.format.fill.solid()
            series.format.fill.fore_color.rgb = RGBColor(*hex_to_rgb(color))
    except Exception:
        pass  # Best-effort styling

    return chart_h + Inches(0.15)


def _numeric_chart_value(value: Any) -> float:
    if isinstance(value, (int, float)):
        return float(value)
    try:
        return float(str(value).replace(",", "").strip())
    except (TypeError, ValueError):
        return 0.0


def _render_image(slide, block: SpecBlock, x, y, max_w, sw, sh) -> int:
    img_path = block.path
    if not img_path or not os.path.exists(img_path):
        # Try relative to cwd
        alt_path = Path.cwd() / (img_path or "")
        if alt_path.exists():
            img_path = str(alt_path)
        else:
            # Text placeholder for missing image
            h = Inches(0.4)
            tf = _make_textbox(slide, x, y, max_w, h)
            p = tf.paragraphs[0]
            p.text = f"[Image: {block.path or 'unknown'}]"
            p.font.size = Pt(14)
            p.font.italic = True
            return h

    # Determine image size
    from PIL import Image as PILImage
    try:
        im = PILImage.open(img_path)
        iw, ih = im.size
    except Exception:
        iw, ih = 800, 600

    aspect = ih / iw if iw > 0 else 0.75
    caption_h = Inches(0.35) if block.caption else Inches(0.1)
    available_h = max(Inches(1.0), sh - y - Inches(0.45) - caption_h)

    target_w = min(max_w, Inches(8.0))
    target_h = int(target_w * aspect)
    if target_h > available_h:
        target_h = int(available_h)
        target_w = int(target_h / aspect) if aspect > 0 else target_w

    try:
        pic = slide.shapes.add_picture(img_path, x, y, target_w, target_h)
        pic.name = "Spec Image"
    except Exception:
        h = Inches(0.4)
        tf = _make_textbox(slide, x, y, max_w, h)
        p = tf.paragraphs[0]
        p.text = f"[Image: {block.path} — could not be embedded]"
        p.font.size = Pt(14)
        p.font.italic = True
        return h

    if block.caption:
        cap_y = y + target_h + Inches(0.05)
        cap_h = Inches(0.3)
        tf = _make_textbox(slide, x, cap_y, max_w, cap_h)
        p = tf.paragraphs[0]
        p.text = block.caption
        p.font.size = Pt(11)
        p.font.italic = True
        return target_h + Inches(0.35)

    return target_h + Inches(0.1)


def _render_formula_block(slide, block: SpecBlock, theme, x, y, max_w) -> int:
    latex = block.latex or block.text or ""
    if not latex:
        source_image = _formula_source_image(block)
        if source_image:
            return _render_formula_source_image(slide, block, source_image, x, y, max_w)
        return 0

    mode = _formula_render_mode(block)
    if mode not in ("image", "png", "formula_image"):
        try:
            return _render_office_math_formula(slide, block, theme, x, y, max_w, latex)
        except Exception:
            pass

    source_image = _formula_source_image(block)
    if source_image:
        rendered = _render_formula_source_image(slide, block, source_image, x, y, max_w)
        if rendered:
            return rendered

    return _render_formula_image(slide, block, theme, x, y, max_w, latex)


def _formula_render_mode(block: SpecBlock) -> str:
    metadata = block.metadata or {}
    return str(
        block.formula_format
        or block.formulaFormat
        or metadata.get("formula_format")
        or metadata.get("formulaFormat")
        or metadata.get("equation_format")
        or metadata.get("render_as")
        or metadata.get("renderAs")
        or "office_math"
    ).lower()


def _formula_source_image(block: SpecBlock) -> str | None:
    metadata = block.metadata or {}
    path = (
        metadata.get("source_image_path")
        or metadata.get("sourceImagePath")
        or metadata.get("fallback_image_path")
        or metadata.get("fallbackImagePath")
    )
    if not path:
        return None
    candidate = Path(str(path))
    if candidate.exists():
        return str(candidate)
    alt = Path.cwd() / str(path)
    return str(alt) if alt.exists() else None


def _render_office_math_formula(slide, block: SpecBlock, theme, x, y, max_w, latex: str) -> int:
    h = Inches(0.72)
    box = slide.shapes.add_textbox(x, y, max_w, h)
    box.name = "Formula Office Math"
    try:
        box._element.nvSpPr.cNvPr.set("descr", latex)
    except Exception:
        pass

    tf = box.text_frame
    tf.clear()
    tf.margin_left = Inches(0.04)
    tf.margin_right = Inches(0.04)
    tf.vertical_anchor = MSO_ANCHOR.MIDDLE

    tx_body = box._element.txBody
    for child in list(tx_body):
        if child.tag == qn("a:p"):
            tx_body.remove(child)
    tx_body.append(parse_xml(latex_to_powerpoint_math_paragraph(latex, font_size_pt=20)))

    consumed = h + Inches(0.08)
    label = block.equation_number or block.caption
    if label:
        cap = _make_textbox(slide, x, y + h + Inches(0.02), max_w, Inches(0.26))
        p = cap.paragraphs[0]
        p.text = label
        p.font.size = Pt(11)
        p.font.italic = True
        p.font.color.rgb = RGBColor(*hex_to_rgb(theme["body_color"]))
        p.alignment = PP_ALIGN.CENTER
        consumed += Inches(0.28)
    return consumed


def _render_formula_source_image(slide, block: SpecBlock, img_path: str, x, y, max_w) -> int:
    try:
        from PIL import Image as PILImage

        im = PILImage.open(img_path)
        iw, ih = im.size
        target_w = min(Inches(5), max_w)
        aspect = ih / iw if iw > 0 else 0.2
        target_h = int(target_w * aspect)
        pic = slide.shapes.add_picture(img_path, x, y, target_w, target_h)
        pic.name = "Formula Source Image"
        return target_h + (Inches(0.35) if (block.caption or block.equation_number) else Inches(0.1))
    except Exception:
        return 0


def _render_formula_image(slide, block: SpecBlock, theme, x, y, max_w, latex: str) -> int:
    import tempfile

    with tempfile.NamedTemporaryFile(suffix=".png", delete=False) as tf:
        formula_path = tf.name
    try:
        render_formula(latex, formula_path)
        if os.path.exists(formula_path):
            from PIL import Image as PILImage
            im = PILImage.open(formula_path)
            iw, ih = im.size
            target_w = min(Inches(5), max_w)
            aspect = ih / iw if iw > 0 else 0.2
            target_h = int(target_w * aspect)
            pic = slide.shapes.add_picture(formula_path, x, y, target_w, target_h)
            pic.name = "Formula Image"
            consumed = target_h + Inches(0.1)
            label = block.equation_number or block.caption
            if label:
                cap = _make_textbox(slide, x, y + target_h + Inches(0.02), max_w, Inches(0.26))
                p = cap.paragraphs[0]
                p.text = label
                p.font.size = Pt(11)
                p.font.italic = True
                p.font.color.rgb = RGBColor(*hex_to_rgb(theme["body_color"]))
                p.alignment = PP_ALIGN.CENTER
                consumed += Inches(0.28)
            return consumed
    finally:
        try:
            os.unlink(formula_path)
        except Exception:
            pass
    return 0


def _render_two_column(slide, block: SpecBlock, theme, x, y, max_w, sw, sh) -> int:
    gap = Inches(0.3)
    col_w = (max_w - gap) // 2
    right_x = x + col_w + gap

    if block.left:
        _render_blocks_at(slide, block.left, theme, x, y, col_w, sw, sh)
    if block.right:
        _render_blocks_at(slide, block.right, theme, right_x, y, col_w, sw, sh)

    # Estimate height: max of left/right block counts * row_h
    left_count = len(block.left or [])
    right_count = len(block.right or [])
    return Inches(0.45 * max(left_count, right_count, 1))


def _render_blocks_at(slide, blocks: list[SpecBlock], theme, x, y, max_w, sw, sh):
    """Render blocks at a specific position (used for two-column)."""
    cy = y
    for b in blocks:
        h = _render_block(slide, b, theme, x, cy, max_w, sw, sh)
        cy += h


def _render_quote(slide, block: SpecBlock, theme, x, y, max_w) -> int:
    h = Inches(0.6)
    tf = _make_textbox(slide, x + Inches(0.2), y, max_w - Inches(0.4), h)
    p = tf.paragraphs[0]
    p.text = f"❝ {block.text or ''} ❞"
    p.font.size = Pt(20)
    p.font.italic = True
    p.font.color.rgb = RGBColor(*hex_to_rgb(theme["accent"]))
    p.alignment = PP_ALIGN.CENTER
    _set_font_name(p, theme["font"])
    if block.attribution:
        p2 = tf.add_paragraph()
        p2.text = f"— {block.attribution}"
        p2.font.size = Pt(14)
        p2.font.color.rgb = RGBColor(*hex_to_rgb(theme["body_color"]))
        p2.alignment = PP_ALIGN.RIGHT
        _set_font_name(p2, theme["font"])
    return Inches(0.8)


def _render_code_block(slide, block: SpecBlock, theme, x, y, max_w) -> int:
    code = block.code or block.text or ""
    lines = code.split("\n")
    h = Inches(0.3 * max(len(lines), 1) + 0.1)
    tf = _make_textbox(slide, x, y, max_w, h)
    p = tf.paragraphs[0]
    p.text = code
    p.font.size = Pt(12)
    p.font.name = "Courier New"
    p.font.color.rgb = RGBColor(*hex_to_rgb(theme["body_color"]))
    return h + Inches(0.1)


# ---------------------------------------------------------------------------
# Layout inspection
# ---------------------------------------------------------------------------

def _inspect_pptx_structure(path: Path) -> dict[str, Any]:
    result: dict[str, Any] = {
        "pptx_structure_checked": True,
        "pptx_picture_count": 0,
        "pptx_spec_image_count": 0,
        "pptx_formula_image_count": 0,
        "pptx_media_file_count": 0,
        "pptx_table_object_count": 0,
        "pptx_spec_table_count": 0,
        "pptx_chart_object_count": 0,
        "pptx_spec_chart_count": 0,
        "pptx_office_math_count": 0,
        "embedded_image_count": 0,
        "rendered_table_count": 0,
        "rendered_chart_count": 0,
        "editable_formula_count": 0,
    }
    try:
        with zipfile.ZipFile(path) as zf:
            names = zf.namelist()
            slide_names = [
                name for name in names
                if name.startswith("ppt/slides/slide") and name.endswith(".xml")
            ]
            result["pptx_media_file_count"] = len([
                name for name in names
                if name.startswith("ppt/media/") and not name.endswith("/")
            ])
            for name in slide_names:
                xml = zf.read(name)
                result["pptx_picture_count"] += len(re.findall(rb"<p:pic\b", xml))
                result["pptx_spec_image_count"] += xml.count(b'name="Spec Image"')
                result["pptx_formula_image_count"] += (
                    xml.count(b'name="Formula Image"') + xml.count(b'name="Formula Source Image"')
                )
                result["pptx_table_object_count"] += len(re.findall(rb"<a:tbl\b", xml))
                result["pptx_spec_table_count"] += xml.count(b'name="Spec Table"')
                result["pptx_chart_object_count"] += xml.count(
                    b"http://schemas.openxmlformats.org/drawingml/2006/chart"
                )
                result["pptx_spec_chart_count"] += xml.count(b'name="Spec Chart"')
                result["pptx_office_math_count"] += len(re.findall(rb"<m:oMath(?:\s|>)", xml))
    except Exception as exc:
        result["pptx_structure_error"] = str(exc)

    result["embedded_image_count"] = result["pptx_spec_image_count"]
    result["rendered_table_count"] = result["pptx_spec_table_count"] or result["pptx_table_object_count"]
    result["rendered_chart_count"] = result["pptx_spec_chart_count"] or result["pptx_chart_object_count"]
    result["editable_formula_count"] = result["pptx_office_math_count"]
    return result


def _inspect_presentation_layout(prs: Presentation) -> list[str]:
    warnings: list[str] = []
    slide_w = int(prs.slide_width)
    slide_h = int(prs.slide_height)
    margin = int(Inches(0.12))

    for slide_idx, slide in enumerate(prs.slides, start=1):
        boxes: list[tuple[int, int, int, int, str]] = []
        for shape in slide.shapes:
            if not all(hasattr(shape, attr) for attr in ("left", "top", "width", "height")):
                continue
            left, top, width, height = int(shape.left), int(shape.top), int(shape.width), int(shape.height)
            if width <= 0 or height <= 0:
                continue
            name = getattr(shape, "name", "shape")
            right = left + width
            bottom = top + height
            if right > slide_w + margin or bottom > slide_h + margin or left < -margin or top < -margin:
                warnings.append(f"slide {slide_idx}: shape '{name}' extends beyond slide bounds")
            boxes.append((left, top, right, bottom, name))

        for i, a in enumerate(boxes):
            for b in boxes[i + 1:]:
                if _boxes_overlap_significantly(a, b):
                    warnings.append(f"slide {slide_idx}: shape '{a[4]}' overlaps '{b[4]}'")
                    if len(warnings) >= 20:
                        return warnings
    return warnings


def _boxes_overlap_significantly(a: tuple[int, int, int, int, str], b: tuple[int, int, int, int, str]) -> bool:
    left = max(a[0], b[0])
    top = max(a[1], b[1])
    right = min(a[2], b[2])
    bottom = min(a[3], b[3])
    if right <= left or bottom <= top:
        return False
    overlap = (right - left) * (bottom - top)
    area_a = max((a[2] - a[0]) * (a[3] - a[1]), 1)
    area_b = max((b[2] - b[0]) * (b[3] - b[1]), 1)
    return overlap / min(area_a, area_b) > 0.18


def _merge_layout_quality(quality: dict, warnings: list[str]) -> None:
    if not warnings:
        quality.setdefault("checks", []).append({
            "id": "pptx.layout",
            "status": "pass",
            "message": "Slide layout inspection found no obvious overflow or overlap.",
        })
        quality["check_count"] = quality.get("check_count", 0) + 1
        return

    quality.setdefault("warnings", []).extend(warnings)
    checks = quality.setdefault("checks", [])
    for warning in warnings:
        checks.append({
            "id": "pptx.layout",
            "status": "warn",
            "message": warning,
        })
    quality["warning_count"] = quality.get("warning_count", 0) + len(warnings)
    quality["check_count"] = quality.get("check_count", 0) + len(warnings)
    _refresh_quality_level(quality)


def _refresh_quality_level(quality: dict) -> None:
    penalty = quality.get("warning_count", 0) * 3 + quality.get("failure_count", 0) * 18 + quality.get("blocker_count", 0) * 30
    quality["quality_score"] = max(0, min(100, 100 - penalty))
    if quality.get("blocker_count", 0):
        quality["quality_level"] = "blocked"
    elif quality.get("failure_count", 0):
        quality["quality_level"] = "failed"
    elif quality.get("warning_count", 0):
        quality["quality_level"] = "degraded"
    else:
        quality["quality_level"] = "final"


# ---------------------------------------------------------------------------
# Manifest
# ---------------------------------------------------------------------------

def _write_manifest(out_path: Path, format: str, quality: dict, spec: DocumentSpec, asset_manifest: dict[str, Any] | None = None) -> str:
    manifest_path = out_path.with_suffix(f".{format}.manifest.json")
    manifest = {
        "filePath": str(out_path),
        "format": format,
        "quality": quality,
        "document": _spec_manifest_summary(spec),
    }
    if asset_manifest:
        manifest["sourceAssetManifest"] = {
            "manifest_path": asset_manifest.get("manifest_path"),
            "source_path": asset_manifest.get("source_path"),
            "complete": asset_manifest.get("complete"),
            "page_count": asset_manifest.get("page_count"),
            "pages_scanned": asset_manifest.get("pages_scanned"),
            "asset_count": asset_manifest.get("asset_count"),
            "asset_inventory": asset_manifest.get("asset_inventory") or [],
        }
    manifest_path.write_text(json.dumps(manifest, indent=2, ensure_ascii=False), encoding="utf-8")
    return str(manifest_path)


def _spec_manifest_summary(spec: DocumentSpec) -> dict[str, Any]:
    contract = spec.generation_contract
    return {
        "title": spec.title,
        "subtitle": spec.subtitle,
        "author": spec.author,
        "language": spec.language,
        "document_type": spec.document_type.value if hasattr(spec.document_type, "value") else str(spec.document_type),
        "audience": spec.audience,
        "source_documents": [s.model_dump(exclude_none=True) for s in spec.source_documents],
        "generation_contract": contract.model_dump(exclude_none=True) if contract else None,
    }
