"""Extract text and structure from existing document files.

Supports PPTX, DOCX, XLSX, and (optionally) PDF extraction.
Returns structured text with slide/page separators, table content, and metadata.
"""

from __future__ import annotations

import json
import os
import re
import time
import contextlib
import io
from pathlib import Path
from typing import Any


EXTRACT_TOOL = {
    "description": (
        "Extract text content and structure from an existing document file. "
        "Supports PPTX (slide-by-slide), DOCX (paragraphs and tables), "
        "XLSX (sheets and cells), and PDF (page-by-page with optional pdfplumber). "
        "Returns structured text including slide/page separators, table content, "
        "and any image alt-text hints found in the document."
    ),
    "inputSchema": {
        "type": "object",
        "properties": {
            "path": {"type": "string", "description": "Path to the document file"},
            "format": {
                "type": "string",
                "enum": ["pptx", "docx", "xlsx", "pdf", "auto"],
                "description": "Document format (auto-detect from extension if 'auto')"
            },
            "include_tables": {"type": "boolean", "default": True},
            "include_image_hints": {"type": "boolean", "default": True},
        },
        "required": ["path"],
    },
}

EXTRACT_ASSETS_TOOL = {
    "description": (
        "Extract source visual assets from an existing document, especially PDF "
        "figures, table crops, and formula candidate crops for academic PPT/DOCX "
        "generation. Saves extracted assets as PNG files and returns source_refs, "
        "crop_box metadata, and suggested DocumentSpec image blocks."
    ),
    "inputSchema": {
        "type": "object",
        "properties": {
            "path": {"type": "string", "description": "Path to the source document"},
            "format": {
                "type": "string",
                "enum": ["pdf", "auto"],
                "description": "Document format (auto-detect from extension if 'auto')",
            },
            "output_dir": {
                "type": "string",
                "default": "output/extracted-assets",
                "description": "Directory where extracted PNG assets and manifest are written",
            },
            "max_pages": {
                "type": "integer",
                "description": "Optional maximum number of pages to inspect",
            },
            "start_page": {
                "type": "integer",
                "default": 1,
                "description": "1-based page number to start scanning from when max_pages is used",
            },
            "page_ranges": {
                "type": "array",
                "items": {"type": "string"},
                "description": "Optional page ranges such as ['2-4', '9', '15-18'] for targeted extraction",
            },
            "resume": {
                "type": "boolean",
                "default": True,
                "description": "Reuse an existing assets.manifest.json and skip pages already scanned",
            },
            "scan_all": {
                "type": "boolean",
                "default": False,
                "description": "Scan every page in one call. Defaults to false so large PDFs are processed in resumable batches.",
            },
            "page_batch_size": {
                "type": "integer",
                "default": 12,
                "description": "Default number of pages to scan when max_pages/page_ranges are not provided.",
            },
            "max_runtime_seconds": {
                "type": "number",
                "default": 45,
                "description": "Soft time budget for one extraction call; writes a resumable manifest before returning.",
            },
            "render_page_previews": {
                "type": "boolean",
                "default": False,
                "description": "Also render whole-page preview images for manual crop selection",
            },
            "extract_images": {"type": "boolean", "default": True},
            "extract_captioned_figures": {
                "type": "boolean",
                "default": True,
                "description": "Crop figure candidates above captions like 图3.2 / Fig. 3.2; useful for vector PDF figures.",
            },
            "extract_tables": {"type": "boolean", "default": True},
            "extract_formula_candidates": {"type": "boolean", "default": True},
            "min_width": {"type": "number", "default": 48},
            "min_height": {"type": "number", "default": 24},
        },
        "required": ["path"],
    },
}


def extract_document_text(args: dict) -> dict:
    """MCP tool handler for document text extraction."""
    path = args["path"]
    fmt = args.get("format", "auto")
    include_tables = args.get("include_tables", True)
    include_image_hints = args.get("include_image_hints", True)

    if not os.path.exists(path):
        return {"error": f"File not found: {path}"}

    if fmt == "auto":
        ext = Path(path).suffix.lower().lstrip(".")
        fmt = {"pptx": "pptx", "ppt": "pptx",
               "docx": "docx", "doc": "docx",
               "xlsx": "xlsx", "xls": "xlsx",
               "pdf": "pdf"}.get(ext, "unknown")

    if fmt == "pptx":
        result = _extract_pptx(path, include_tables, include_image_hints)
    elif fmt == "docx":
        result = _extract_docx(path, include_tables, include_image_hints)
    elif fmt == "xlsx":
        result = _extract_xlsx(path, include_tables)
    elif fmt == "pdf":
        result = _extract_pdf(path, include_tables)
    else:
        return {"error": f"Unsupported format: {fmt}"}

    return result


def extract_document_assets(args: dict) -> dict:
    """MCP tool handler for PDF asset extraction.

    The output is intentionally DocumentSpec-shaped: each extracted PNG includes
    a suggested `image` block with source_path/crop_box/source_refs metadata so
    downstream deck generation can embed original paper evidence instead of
    inventing approximate diagrams or tables.
    """
    path = args["path"]
    fmt = args.get("format", "auto")

    if not os.path.exists(path):
        return {"error": f"File not found: {path}"}

    if fmt == "auto":
        fmt = "pdf" if Path(path).suffix.lower() == ".pdf" else "unknown"
    if fmt != "pdf":
        return {"error": f"Unsupported asset extraction format: {fmt}"}

    return _extract_pdf_assets(
        path=path,
        output_dir=args.get("output_dir", "output/extracted-assets"),
        max_pages=args.get("max_pages"),
        start_page=int(args.get("start_page", 1) or 1),
        page_ranges=args.get("page_ranges"),
        resume=bool(args.get("resume", True)),
        scan_all=bool(args.get("scan_all", False)),
        page_batch_size=int(args.get("page_batch_size", 12) or 12),
        max_runtime_seconds=float(args.get("max_runtime_seconds", 45) or 0),
        render_page_previews=bool(args.get("render_page_previews", False)),
        extract_images=bool(args.get("extract_images", True)),
        extract_captioned_figures=bool(args.get("extract_captioned_figures", True)),
        extract_tables=bool(args.get("extract_tables", True)),
        extract_formula_candidates=bool(args.get("extract_formula_candidates", True)),
        min_width=float(args.get("min_width", 48)),
        min_height=float(args.get("min_height", 24)),
    )


def _extract_pptx(path: str, include_tables: bool, include_image_hints: bool) -> dict:
    from pptx import Presentation
    prs = Presentation(path)
    slides = []
    total_chars = 0

    for si, slide in enumerate(prs.slides):
        slide_text_parts = []
        for shape in slide.shapes:
            if shape.has_text_frame:
                for para in shape.text_frame.paragraphs:
                    text = para.text.strip()
                    if text:
                        slide_text_parts.append(text)
            if shape.has_table and include_tables:
                table = shape.table
                for row in table.rows:
                    cells = [cell.text.strip() for cell in row.cells]
                    slide_text_parts.append(" | ".join(cells))
            if include_image_hints and shape.shape_type == 13:  # Picture
                alt = shape._element.get('descr', '')
                if alt:
                    slide_text_parts.append(f"[Image: {alt}]")

        slide_text = "\n".join(slide_text_parts)
        total_chars += len(slide_text)
        slides.append({
            "slide": si + 1,
            "text": slide_text,
            "char_count": len(slide_text),
        })

    return {
        "format": "pptx",
        "slide_count": len(prs.slides),
        "total_chars": total_chars,
        "slides": slides,
        "full_text": "\n\n--- Slide Break ---\n\n".join(s["text"] for s in slides),
    }


def _extract_docx(path: str, include_tables: bool, include_image_hints: bool) -> dict:
    from docx import Document
    doc = Document(path)
    paragraphs = []
    tables = []
    total_chars = 0

    for para in doc.paragraphs:
        text = para.text.strip()
        if text:
            paragraphs.append({"style": para.style.name if para.style else "", "text": text})
            total_chars += len(text)

    if include_tables:
        for ti, table in enumerate(doc.tables):
            table_data = []
            for row in table.rows:
                cells = [cell.text.strip() for cell in row.cells]
                table_data.append(cells)
            tables.append({"index": ti, "rows": len(table_data), "data": table_data})

    full_text = "\n\n".join(p["text"] for p in paragraphs)

    return {
        "format": "docx",
        "paragraph_count": len(paragraphs),
        "table_count": len(tables),
        "total_chars": total_chars,
        "paragraphs": paragraphs,
        "tables": tables,
        "full_text": full_text,
    }


def _extract_xlsx(path: str, include_tables: bool) -> dict:
    from openpyxl import load_workbook
    wb = load_workbook(path, data_only=True)
    sheets = []

    for sheet_name in wb.sheetnames:
        ws = wb[sheet_name]
        rows = []
        for row in ws.iter_rows(values_only=True):
            str_row = [str(cell) if cell is not None else "" for cell in row]
            if any(str_row):
                rows.append(str_row)
        sheets.append({
            "name": sheet_name,
            "row_count": len(rows),
            "col_count": max((len(r) for r in rows), default=0),
            "data": rows,
        })

    total_cells = sum(s["row_count"] * s["col_count"] for s in sheets)
    return {
        "format": "xlsx",
        "sheet_count": len(sheets),
        "total_cells": total_cells,
        "sheets": sheets,
    }


def _extract_pdf(path: str, include_tables: bool) -> dict:
    try:
        import pdfplumber
        with pdfplumber.open(path) as pdf:
            pages = []
            total_chars = 0
            for pi, page in enumerate(pdf.pages):
                text = page.extract_text() or ""
                total_chars += len(text)

                page_data = {"page": pi + 1, "text": text, "char_count": len(text)}

                if include_tables:
                    tables = page.extract_tables()
                    if tables:
                        page_data["tables"] = [[[str(c) if c else "" for c in row] for row in tbl] for tbl in tables]

                pages.append(page_data)

        return {
            "format": "pdf",
            "page_count": len(pages),
            "total_chars": total_chars,
            "pages": pages,
            "full_text": "\n\n--- Page Break ---\n\n".join(p["text"] for p in pages),
        }
    except ImportError:
        # Fallback: try PyPDF2 or similar
        return {"error": "pdfplumber not installed. Install with: pip install pdfplumber"}


def _extract_pdf_assets(
    path: str,
    output_dir: str,
    max_pages: int | None,
    start_page: int,
    page_ranges: list[str] | None,
    resume: bool,
    scan_all: bool,
    page_batch_size: int,
    max_runtime_seconds: float,
    render_page_previews: bool,
    extract_images: bool,
    extract_captioned_figures: bool,
    extract_tables: bool,
    extract_formula_candidates: bool,
    min_width: float,
    min_height: float,
) -> dict:
    try:
        import fitz  # PyMuPDF
    except ImportError:
        return {
            "format": "pdf",
            "asset_count": 0,
            "assets": [],
            "warnings": [
                "PyMuPDF is not installed. Install himalaya-doc-service[extract] or pip install PyMuPDF to extract PDF figures and formula crops."
            ],
        }

    warnings: list[str] = []
    assets: list[dict[str, Any]] = []
    source_path = Path(path)
    asset_root = Path(output_dir) / source_path.stem
    asset_root.mkdir(parents=True, exist_ok=True)
    manifest_path = asset_root / "assets.manifest.json"

    scanned_pages: set[int] = set()
    if resume and manifest_path.exists():
        try:
            previous = json.loads(manifest_path.read_text(encoding="utf-8"))
            assets.extend(previous.get("assets") or [])
            scanned_pages.update(max(0, int(p) - 1) for p in previous.get("scanned_page_numbers") or [])
        except Exception as exc:
            warnings.append(f"Existing asset manifest could not be reused: {exc}")

    doc = fitz.open(path)
    started_at = time.monotonic()
    page_numbers = _resolve_page_numbers(
        len(doc),
        start_page,
        max_pages,
        page_ranges,
        page_batch_size=page_batch_size,
        scan_all=scan_all,
    )
    requested_page_numbers = list(page_numbers)
    if resume:
        page_numbers = [page for page in page_numbers if page not in scanned_pages]

    if not scan_all and not max_pages and not page_ranges:
        warnings.append(
            f"Large-PDF safe mode: scanned at most {page_batch_size} page(s) in this call. "
            "Use next_start_page/resume for the next batch, or pass scan_all=true explicitly."
        )

    table_bboxes = (
        _detect_pdf_table_bboxes(path, page_numbers, warnings)
        if extract_tables and page_numbers else {}
    )
    table_records = (
        _extract_pdf_table_records(path, page_numbers, warnings)
        if extract_tables and page_numbers else {}
    )

    for page_index in page_numbers:
        if max_runtime_seconds > 0 and time.monotonic() - started_at > max_runtime_seconds:
            warnings.append(
                f"Stopped after the {max_runtime_seconds:.0f}s extraction budget; "
                "resume with next_start_page to continue."
            )
            break

        page = doc.load_page(page_index)
        page_no = page_index + 1
        page_assets = 0
        page_asset_bboxes: list[list[float]] = []
        page_table_bboxes: list[list[float]] = []

        if render_page_previews:
            out = asset_root / f"page-{page_no:03d}-preview.png"
            pix = page.get_pixmap(matrix=fitz.Matrix(1.5, 1.5), alpha=False)
            pix.save(str(out))
            assets.append(_asset_record(
                role="page_preview",
                out_path=out,
                source_path=source_path,
                page_no=page_no,
                bbox=[page.rect.x0, page.rect.y0, page.rect.x1, page.rect.y1],
                width=pix.width,
                height=pix.height,
                text=None,
                caption=None,
            ))
            page_assets += 1

        if extract_images:
            image_index = 0
            for block in page.get_text("dict").get("blocks", []):
                if block.get("type") != 1:
                    continue
                bbox = block.get("bbox")
                if not _bbox_is_large_enough(bbox, min_width, min_height):
                    continue
                image_index += 1
                out = asset_root / f"page-{page_no:03d}-figure-{image_index:02d}.png"
                saved = _save_page_crop(page, bbox, out, zoom=2.0)
                if not saved:
                    continue
                caption = _caption_near_bbox(page, bbox, role="figure")
                assets.append(_asset_record(
                    role="figure",
                    out_path=out,
                    source_path=source_path,
                    page_no=page_no,
                    bbox=bbox,
                    width=saved["width"],
                    height=saved["height"],
                    text=None,
                    caption=caption,
                ))
                page_asset_bboxes.append([float(v) for v in bbox])
                page_assets += 1

        if extract_captioned_figures:
            for figure_index, (bbox, caption) in enumerate(
                _find_captioned_figure_bboxes(page, min_width, min_height),
                start=1,
            ):
                if any(_bbox_iou(bbox, existing) > 0.60 for existing in page_asset_bboxes):
                    continue
                out = asset_root / f"page-{page_no:03d}-caption-figure-{figure_index:02d}.png"
                saved = _save_page_crop(page, bbox, out, zoom=2.0)
                if not saved:
                    continue
                assets.append(_asset_record(
                    role="figure",
                    out_path=out,
                    source_path=source_path,
                    page_no=page_no,
                    bbox=bbox,
                    width=saved["width"],
                    height=saved["height"],
                    text=_short_text(caption),
                    caption=_short_text(caption),
                    extraction_method="caption_crop",
                ))
                page_asset_bboxes.append([float(v) for v in bbox])
                page_assets += 1

        if extract_tables:
            for table_index, bbox in enumerate(table_bboxes.get(page_no, []), start=1):
                if not _bbox_is_large_enough(bbox, min_width, min_height):
                    continue
                out = asset_root / f"page-{page_no:03d}-table-{table_index:02d}.png"
                saved = _save_page_crop(page, bbox, out, zoom=2.0)
                if not saved:
                    continue
                caption = _caption_near_bbox(page, bbox, role="table")
                table_record = _matching_table_record(table_records.get(page_no, []), bbox)
                assets.append(_asset_record(
                    role="table_image",
                    out_path=out,
                    source_path=source_path,
                    page_no=page_no,
                    bbox=bbox,
                    width=saved["width"],
                    height=saved["height"],
                    text=None,
                    caption=caption,
                    table_data=table_record,
                ))
                page_table_bboxes.append([float(v) for v in bbox])
                page_assets += 1

            if not page_table_bboxes:
                for table_index, (bbox, caption) in enumerate(
                    _find_captioned_table_bboxes(page, min_width, min_height),
                    start=1,
                ):
                    out = asset_root / f"page-{page_no:03d}-caption-table-{table_index:02d}.png"
                    saved = _save_page_crop(page, bbox, out, zoom=2.0)
                    if not saved:
                        continue
                    assets.append(_asset_record(
                        role="table_image",
                        out_path=out,
                        source_path=source_path,
                        page_no=page_no,
                        bbox=bbox,
                        width=saved["width"],
                        height=saved["height"],
                        text=_short_text(caption),
                        caption=_short_text(caption),
                        extraction_method="caption_table_crop",
                    ))
                    page_table_bboxes.append([float(v) for v in bbox])
                    page_assets += 1

        if extract_formula_candidates:
            formula_index = 0
            for text_block in page.get_text("blocks"):
                if len(text_block) < 5:
                    continue
                x0, y0, x1, y1, text = text_block[:5]
                bbox = [float(x0), float(y0), float(x1), float(y1)]
                if not _bbox_is_large_enough(bbox, min_width, min(min_height, 8.0)):
                    continue
                if not _looks_like_formula_candidate(str(text)):
                    continue
                formula_index += 1
                out = asset_root / f"page-{page_no:03d}-formula-{formula_index:02d}.png"
                crop_bbox = _expand_bbox(bbox, page.rect, pad_x=8.0, pad_y=6.0)
                saved = _save_page_crop(page, crop_bbox, out, zoom=2.5)
                if not saved:
                    continue
                short = _short_text(str(text))
                assets.append(_asset_record(
                    role="formula_candidate",
                    out_path=out,
                    source_path=source_path,
                    page_no=page_no,
                    bbox=crop_bbox,
                    width=saved["width"],
                    height=saved["height"],
                    text=short,
                    caption=short,
                    formula_latex=_formula_text_to_latex(short),
                ))
                page_assets += 1

        if page_assets == 0:
            warnings.append(f"No visual assets detected on page {page_no}.")
        scanned_pages.add(page_index)
        _write_asset_manifest(
            manifest_path=manifest_path,
            source_path=source_path,
            page_count=len(doc),
            scanned_pages=scanned_pages,
            complete=False,
            assets=assets,
            warnings=warnings,
            requested_pages=requested_page_numbers,
        )

    scanned_sorted = sorted(scanned_pages)
    complete = len(scanned_sorted) >= len(doc)
    remaining_requested = [page for page in requested_page_numbers if page not in scanned_pages]
    next_start_page = (
        remaining_requested[0] + 1
        if remaining_requested
        else ((scanned_sorted[-1] + 2) if scanned_sorted and not complete else None)
    )
    result = _write_asset_manifest(
        manifest_path=manifest_path,
        source_path=source_path,
        page_count=len(doc),
        scanned_pages=scanned_pages,
        complete=complete,
        assets=assets,
        warnings=warnings,
        next_start_page=next_start_page,
        requested_pages=requested_page_numbers,
    )
    doc.close()
    return result


def _write_asset_manifest(
    manifest_path: Path,
    source_path: Path,
    page_count: int,
    scanned_pages: set[int],
    complete: bool,
    assets: list[dict[str, Any]],
    warnings: list[str],
    next_start_page: int | None = None,
    requested_pages: list[int] | None = None,
) -> dict:
    scanned_sorted = sorted(scanned_pages)
    if next_start_page is None and scanned_sorted and not complete:
        next_start_page = scanned_sorted[-1] + 2
    requested_numbers = [page + 1 for page in requested_pages or []]
    result = {
        "format": "pdf",
        "source_path": str(source_path),
        "page_count": page_count,
        "pages_scanned": len(scanned_sorted),
        "scanned_page_numbers": [page + 1 for page in scanned_sorted],
        "requested_page_numbers": requested_numbers,
        "next_start_page": next_start_page,
        "next_call": None if complete or next_start_page is None else {
            "path": str(source_path),
            "format": "pdf",
            "start_page": next_start_page,
            "resume": True,
        },
        "complete": complete,
        "asset_count": len(assets),
        "assets": assets,
        "asset_inventory": [_asset_inventory_item(asset) for asset in assets],
        "suggested_image_blocks": [
            asset["suggested_block"] for asset in assets
            if isinstance(asset.get("suggested_block"), dict)
        ],
        "suggested_table_blocks": [
            asset["suggested_table_block"] for asset in assets
            if isinstance(asset.get("suggested_table_block"), dict)
        ],
        "suggested_formula_blocks": [
            asset["suggested_formula_block"] for asset in assets
            if isinstance(asset.get("suggested_formula_block"), dict)
        ],
        "suggested_blocks": [
            block
            for asset in assets
            for block in (
                asset.get("suggested_table_block"),
                asset.get("suggested_formula_block"),
                asset.get("suggested_block"),
            )
            if isinstance(block, dict)
        ],
        "manifest_path": str(manifest_path),
        "warnings": warnings,
    }
    manifest_path.write_text(json.dumps(result, ensure_ascii=False, indent=2), encoding="utf-8")
    return result


def _asset_inventory_item(asset: dict[str, Any]) -> dict[str, Any]:
    return {
        "page": asset.get("page"),
        "role": asset.get("role"),
        "path": asset.get("path"),
        "caption": asset.get("caption") or asset.get("text"),
        "width": asset.get("width"),
        "height": asset.get("height"),
        "extraction_method": asset.get("extraction_method"),
        "latex": asset.get("latex"),
        "has_structured_table": bool(asset.get("suggested_table_block")),
    }


def _resolve_page_numbers(
    total_pages: int,
    start_page: int,
    max_pages: int | None,
    page_ranges: list[str] | None,
    page_batch_size: int,
    scan_all: bool,
) -> list[int]:
    if page_ranges:
        pages: set[int] = set()
        for item in page_ranges:
            text = str(item).strip()
            if not text:
                continue
            if "-" in text:
                left, right = text.split("-", 1)
                try:
                    start = max(1, int(left.strip()))
                    end = min(total_pages, int(right.strip()))
                except ValueError:
                    continue
                pages.update(range(start - 1, end))
            else:
                try:
                    page = int(text)
                except ValueError:
                    continue
                if 1 <= page <= total_pages:
                    pages.add(page - 1)
        return sorted(pages)

    start_index = max(0, min(total_pages, start_page - 1))
    end_index = total_pages
    effective_max_pages = max_pages
    if effective_max_pages is None and not scan_all:
        effective_max_pages = max(1, page_batch_size)
    if effective_max_pages:
        end_index = min(total_pages, start_index + int(effective_max_pages))
    return list(range(start_index, end_index))


def _detect_pdf_table_bboxes(path: str, page_numbers: list[int], warnings: list[str]) -> dict[int, list[list[float]]]:
    table_bboxes: dict[int, list[list[float]]] = {}
    remaining_pages = list(page_numbers)

    try:
        import fitz

        with fitz.open(path) as doc:
            for page_index in page_numbers:
                if page_index < 0 or page_index >= len(doc):
                    continue
                page = doc.load_page(page_index)
                finder = getattr(page, "find_tables", None)
                if not callable(finder):
                    continue
                with contextlib.redirect_stdout(io.StringIO()), contextlib.redirect_stderr(io.StringIO()):
                    table_result = finder()
                tables = getattr(table_result, "tables", []) or []
                bboxes = [
                    [float(v) for v in table.bbox]
                    for table in tables
                    if getattr(table, "bbox", None)
                ]
                if bboxes:
                    table_bboxes[page_index + 1] = _dedupe_bboxes(bboxes)
    except Exception as exc:
        warnings.append(f"PyMuPDF table detection failed: {exc}")

    remaining_pages = [
        page_index for page_index in remaining_pages
        if (page_index + 1) not in table_bboxes
    ]
    if not remaining_pages:
        return table_bboxes

    try:
        import pdfplumber
    except ImportError:
        if not table_bboxes:
            warnings.append("pdfplumber is not installed; table crop detection skipped.")
        return table_bboxes

    try:
        with pdfplumber.open(path) as pdf:
            for page_index in remaining_pages:
                if page_index < 0 or page_index >= len(pdf.pages):
                    continue
                page = pdf.pages[page_index]
                bboxes: list[list[float]] = []
                for table in page.find_tables() or []:
                    if table.bbox:
                        bboxes.append([float(v) for v in table.bbox])
                if bboxes:
                    existing = table_bboxes.get(page_index + 1, [])
                    table_bboxes[page_index + 1] = _dedupe_bboxes(existing + bboxes)
    except Exception as exc:  # best-effort extraction
        warnings.append(f"Table crop detection failed: {exc}")
    return table_bboxes


def _extract_pdf_table_records(path: str, page_numbers: list[int], warnings: list[str]) -> dict[int, list[dict[str, Any]]]:
    records: dict[int, list[dict[str, Any]]] = {}

    try:
        import fitz

        with fitz.open(path) as doc:
            for page_index in page_numbers:
                if page_index < 0 or page_index >= len(doc):
                    continue
                page = doc.load_page(page_index)
                finder = getattr(page, "find_tables", None)
                if not callable(finder):
                    continue
                with contextlib.redirect_stdout(io.StringIO()), contextlib.redirect_stderr(io.StringIO()):
                    table_result = finder()
                for table in getattr(table_result, "tables", []) or []:
                    bbox = getattr(table, "bbox", None)
                    extract = getattr(table, "extract", None)
                    if not bbox or not callable(extract):
                        continue
                    structured = _normalise_table_rows(extract() or [])
                    if not structured:
                        continue
                    records.setdefault(page_index + 1, []).append({
                        "bbox": [float(v) for v in bbox],
                        **structured,
                    })
    except Exception as exc:
        warnings.append(f"PyMuPDF structured table extraction failed: {exc}")

    try:
        import pdfplumber
    except ImportError:
        return records

    try:
        with pdfplumber.open(path) as pdf:
            for page_index in page_numbers:
                if page_index < 0 or page_index >= len(pdf.pages):
                    continue
                page_records: list[dict[str, Any]] = []
                for table in page.find_tables() or []:
                    if not table.bbox:
                        continue
                    raw = table.extract() or []
                    structured = _normalise_table_rows(raw)
                    if not structured:
                        continue
                    page_records.append({
                        "bbox": [float(v) for v in table.bbox],
                        **structured,
                    })
                if page_records:
                    existing = records.get(page_index + 1, [])
                    records[page_index + 1] = _dedupe_table_records(existing + page_records)
    except Exception as exc:
        warnings.append(f"Structured table extraction failed: {exc}")
    return records


def _normalise_table_rows(raw_rows: list[list[Any]]) -> dict[str, Any] | None:
    rows: list[list[str]] = []
    for row in raw_rows or []:
        cells = [str(cell).strip() if cell is not None else "" for cell in row]
        if any(cells):
            rows.append(cells)
    if not rows:
        return None

    width = max(len(row) for row in rows)
    rows = [row + [""] * (width - len(row)) for row in rows]
    headers = rows[0] if len(rows) > 1 and any(rows[0]) else []
    body = rows[1:] if headers else rows
    return {"headers": headers, "rows": body}


def _matching_table_record(records: list[dict[str, Any]], bbox: list[float]) -> dict[str, Any] | None:
    best: tuple[float, dict[str, Any]] | None = None
    for record in records:
        rbbox = record.get("bbox")
        if not rbbox:
            continue
        score = _bbox_iou(bbox, rbbox)
        if best is None or score > best[0]:
            best = (score, record)
    return best[1] if best and best[0] > 0.20 else None


def _dedupe_table_records(records: list[dict[str, Any]], iou_threshold: float = 0.80) -> list[dict[str, Any]]:
    deduped: list[dict[str, Any]] = []
    for record in records:
        bbox = record.get("bbox")
        if bbox and any(_bbox_iou(bbox, existing.get("bbox")) > iou_threshold for existing in deduped if existing.get("bbox")):
            continue
        deduped.append(record)
    return deduped


def _dedupe_bboxes(bboxes: list[list[float]], iou_threshold: float = 0.80) -> list[list[float]]:
    deduped: list[list[float]] = []
    for bbox in bboxes:
        if any(_bbox_iou(bbox, existing) > iou_threshold for existing in deduped):
            continue
        deduped.append(bbox)
    return deduped


def _expand_bbox(bbox: list[float], page_rect, pad_x: float, pad_y: float) -> list[float]:
    x0, y0, x1, y1 = [float(v) for v in bbox]
    return [
        max(float(page_rect.x0), x0 - pad_x),
        max(float(page_rect.y0), y0 - pad_y),
        min(float(page_rect.x1), x1 + pad_x),
        min(float(page_rect.y1), y1 + pad_y),
    ]


def _save_page_crop(page, bbox: list[float], out_path: Path, zoom: float = 2.0) -> dict[str, int] | None:
    try:
        import fitz

        rect = fitz.Rect(float(bbox[0]), float(bbox[1]), float(bbox[2]), float(bbox[3]))
        rect = rect & page.rect
        if rect.is_empty or rect.width <= 0 or rect.height <= 0:
            return None
        pix = page.get_pixmap(matrix=fitz.Matrix(zoom, zoom), clip=rect, alpha=False)
        pix.save(str(out_path))
        return {"width": pix.width, "height": pix.height}
    except Exception:
        return None


def _bbox_is_large_enough(bbox: Any, min_width: float, min_height: float) -> bool:
    if not bbox or len(bbox) != 4:
        return False
    try:
        x0, y0, x1, y1 = [float(v) for v in bbox]
    except (TypeError, ValueError):
        return False
    return (x1 - x0) >= min_width and (y1 - y0) >= min_height


def _asset_record(
    role: str,
    out_path: Path,
    source_path: Path,
    page_no: int,
    bbox: list[float],
    width: int,
    height: int,
    text: str | None,
    caption: str | None,
    extraction_method: str | None = None,
    table_data: dict[str, Any] | None = None,
    formula_latex: str | None = None,
) -> dict[str, Any]:
    crop_box = [round(float(v), 2) for v in bbox]
    source_ref = {
        "document": source_path.name,
        "page": page_no,
        "asset_path": str(out_path),
    }
    if role == "figure":
        source_ref["figure"] = caption or f"page {page_no} extracted figure"
    elif role == "table_image":
        source_ref["table"] = caption or f"page {page_no} extracted table"
    elif role == "formula_candidate":
        source_ref["equation"] = text or caption or f"page {page_no} formula candidate"

    suggested_block = {
        "type": "image",
        "path": str(out_path),
        "source_path": str(source_path),
        "crop_box": crop_box,
        "crop_units": "pt",
        "asset_role": role,
        "alt": text or f"{role.replace('_', ' ')} extracted from page {page_no}",
        "caption": caption or f"{source_path.name} p.{page_no} {role.replace('_', ' ')}",
        "source_refs": [source_ref],
    }
    record = {
        "type": "image",
        "role": role,
        "path": str(out_path),
        "source_path": str(source_path),
        "page": page_no,
        "bbox": crop_box,
        "crop_box": crop_box,
        "crop_units": "pt",
        "width": width,
        "height": height,
        "text": text,
        "caption": caption,
        "source_refs": [source_ref],
        "suggested_block": suggested_block,
    }
    if extraction_method:
        record["extraction_method"] = extraction_method
        suggested_block.setdefault("metadata", {})["extraction_method"] = extraction_method
    if table_data and role == "table_image":
        table_block = {
            "type": "table",
            "caption": caption or f"{source_path.name} p.{page_no} extracted table",
            "headers": table_data.get("headers") or [],
            "rows": table_data.get("rows") or [],
            "source_path": str(source_path),
            "crop_box": crop_box,
            "crop_units": "pt",
            "source_refs": [source_ref],
            "metadata": {
                "source_image_path": str(out_path),
                "extraction_method": extraction_method or "pdf_table",
            },
        }
        record["suggested_table_block"] = table_block
    if formula_latex and role == "formula_candidate":
        formula_block = {
            "type": "formula",
            "latex": formula_latex,
            "display": text or caption or formula_latex,
            "caption": caption,
            "source_path": str(source_path),
            "crop_box": crop_box,
            "crop_units": "pt",
            "source_refs": [source_ref],
            "metadata": {
                "source_image_path": str(out_path),
                "extracted_text": text,
                "render_as": "office_math",
            },
        }
        record["latex"] = formula_latex
        record["suggested_formula_block"] = formula_block
    return record


FIGURE_CAPTION_RE = re.compile(
    r"^\s*(图|圖|Fig\.?|Figure)\s*[\dIVXivx]+(?:[.\-．]\d+)*",
    re.IGNORECASE,
)

TABLE_CAPTION_RE = re.compile(
    r"^\s*(表|Table)\s*[\dIVXivx]+(?:[.\-．]\d+)*",
    re.IGNORECASE,
)


def _find_captioned_figure_bboxes(page, min_width: float, min_height: float) -> list[tuple[list[float], str]]:
    candidates: list[tuple[list[float], str]] = []
    page_rect = page.rect
    blocks = sorted(page.get_text("blocks"), key=lambda item: (float(item[1]), float(item[0])))
    for block in blocks:
        if len(block) < 5:
            continue
        x0, y0, x1, y1, text = block[:5]
        caption = _short_text(str(text))
        if not FIGURE_CAPTION_RE.search(caption):
            continue
        crop_h = min(float(page_rect.height) * 0.42, 320.0)
        top = max(float(page_rect.y0) + 18.0, float(y0) - crop_h)
        bottom = max(top + min_height, float(y0) - 6.0)
        bbox = [
            float(page_rect.x0) + 28.0,
            top,
            float(page_rect.x1) - 28.0,
            min(bottom, float(page_rect.y1) - 18.0),
        ]
        if _bbox_is_large_enough(bbox, min_width, min_height):
            candidates.append((bbox, caption))
    return candidates


def _find_captioned_table_bboxes(page, min_width: float, min_height: float) -> list[tuple[list[float], str]]:
    candidates: list[tuple[list[float], str]] = []
    page_rect = page.rect
    blocks = sorted(page.get_text("blocks"), key=lambda item: (float(item[1]), float(item[0])))
    for block in blocks:
        if len(block) < 5:
            continue
        x0, y0, x1, y1, text = block[:5]
        caption = _short_text(str(text))
        if not TABLE_CAPTION_RE.search(caption):
            continue
        crop_h = min(float(page_rect.height) * 0.34, 260.0)
        top = min(float(page_rect.y1) - min_height, float(y1) + 4.0)
        bottom = min(float(page_rect.y1) - 18.0, top + crop_h)
        bbox = [
            float(page_rect.x0) + 28.0,
            top,
            float(page_rect.x1) - 28.0,
            bottom,
        ]
        if _bbox_is_large_enough(bbox, min_width, min_height):
            candidates.append((bbox, caption))
    return candidates


def _caption_near_bbox(page, bbox: list[float], role: str) -> str | None:
    if not bbox or len(bbox) != 4:
        return None
    x0, y0, x1, y1 = [float(v) for v in bbox]
    caption_re = TABLE_CAPTION_RE if role == "table" else FIGURE_CAPTION_RE
    best: tuple[float, str] | None = None
    for block in page.get_text("blocks"):
        if len(block) < 5:
            continue
        bx0, by0, bx1, by1, text = block[:5]
        caption = _short_text(str(text))
        if not caption_re.search(caption):
            continue
        overlaps_x = min(x1, float(bx1)) - max(x0, float(bx0)) > 0
        if not overlaps_x:
            continue
        distance = min(abs(float(by0) - y1), abs(y0 - float(by1)))
        if best is None or distance < best[0]:
            best = (distance, caption)
    return best[1] if best else None


def _bbox_iou(left: list[float], right: list[float]) -> float:
    lx0, ly0, lx1, ly1 = [float(v) for v in left]
    rx0, ry0, rx1, ry1 = [float(v) for v in right]
    ix0, iy0 = max(lx0, rx0), max(ly0, ry0)
    ix1, iy1 = min(lx1, rx1), min(ly1, ry1)
    iw, ih = max(0.0, ix1 - ix0), max(0.0, iy1 - iy0)
    inter = iw * ih
    if inter <= 0:
        return 0.0
    left_area = max(0.0, lx1 - lx0) * max(0.0, ly1 - ly0)
    right_area = max(0.0, rx1 - rx0) * max(0.0, ry1 - ry0)
    union = left_area + right_area - inter
    return inter / union if union > 0 else 0.0


def _looks_like_formula_candidate(text: str) -> bool:
    compact = " ".join(text.split())
    if not compact or len(compact) > 220:
        return False
    math_markers = [
        "=", "+", "-", "×", "÷", "/", "∑", "∏", "∫", "√", "≤", "≥", "≠",
        "λ", "β", "θ", "ξ", "α", "γ", "δ", "μ", "σ", "φ", "式", "公式",
        "Eq.", "eq.", "equation",
    ]
    marker_count = sum(1 for marker in math_markers if marker in compact)
    has_digit = any(ch.isdigit() for ch in compact)
    return marker_count >= 2 or (marker_count >= 1 and has_digit)


def _formula_text_to_latex(text: str) -> str | None:
    compact = _short_text(text, limit=220)
    if not compact:
        return None
    replacements = {
        "\u2211": r"\sum",
        "\u220f": r"\prod",
        "\u222b": r"\int",
        "\u221a": r"\sqrt",
        "\u2264": r"\leq",
        "\u2265": r"\geq",
        "\u2260": r"\neq",
        "\u00d7": r"\times",
        "\u00f7": r"\div",
        "\u00b7": r"\cdot",
        "\u03b1": r"\alpha",
        "\u03b2": r"\beta",
        "\u03b3": r"\gamma",
        "\u03b4": r"\delta",
        "\u03b8": r"\theta",
        "\u03bb": r"\lambda",
        "\u03bc": r"\mu",
        "\u03c3": r"\sigma",
        "\u03c6": r"\phi",
    }
    for src, dst in replacements.items():
        compact = compact.replace(src, dst)
    compact = re.sub(r"^(式|公式|Equation|Eq\.?)\s*[\(\d.\-一二三四五六七八九十]*[:：]?\s*", "", compact, flags=re.IGNORECASE)
    return compact.strip() or None


def _short_text(text: str, limit: int = 160) -> str:
    compact = " ".join(text.split())
    return compact if len(compact) <= limit else compact[: limit - 1] + "…"
