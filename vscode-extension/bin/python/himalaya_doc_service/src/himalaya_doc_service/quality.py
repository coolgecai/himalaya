"""Quality assessment for generated documents.

Evaluates structural completeness, content density, and format-specific
quality metrics. Returns a QualityReport compatible with the Rust
file-generate crate's format.
"""

from __future__ import annotations

import os
import re
from typing import Any

from .spec import BlockType, DocumentSpec, DocumentType, SpecBlock


class QualityStatus:
    PASS = "pass"
    WARN = "warn"
    FAIL = "fail"
    BLOCKER = "blocker"


def assess_document(format: str, spec: DocumentSpec, file_path: str | None = None, **extra) -> dict[str, Any]:
    """Run all quality checks and return a QualityReport dict.

    The returned dict is compatible with the Rust GenerateReport.quality format:
        {"block_count", "table_count", "formula_count", "chart_count",
         "image_count", "check_count", "warning_count", "checks": [...], "warnings": [...]}
    """
    all_blocks = list(_iter_blocks(spec.blocks))
    report = {
        "block_count": len(all_blocks),
        "table_count": 0,
        "formula_count": 0,
        "formula_image_count": 0,
        "chart_count": 0,
        "image_count": 0,
        "slide_count": 0,
        "source_ref_count": 0,
        "speaker_note_count": 0,
        "check_count": 0,
        "warning_count": 0,
        "failure_count": 0,
        "blocker_count": 0,
        "quality_level": "",
        "quality_score": 100,
        "editable_formula_count": 0,
        "rendered_formula_count": 0,
        "latex_only_formula_count": 0,
        "embedded_image_count": 0,
        "rendered_table_count": 0,
        "rendered_chart_count": 0,
        "pptx_structure_checked": False,
        "pptx_picture_count": 0,
        "pptx_table_object_count": 0,
        "pptx_office_math_count": 0,
        "asset_manifest_checked": False,
        "asset_manifest_complete": None,
        "asset_manifest_asset_count": 0,
        "asset_manifest_figure_count": 0,
        "asset_manifest_table_count": 0,
        "asset_manifest_formula_count": 0,
        "source_extracted_figure_count": 0,
        "source_extracted_table_count": 0,
        "source_extracted_formula_count": 0,
        "source_grounded_chart_count": 0,
        "checks": [],
        "warnings": [],
        "failures": [],
        "blockers": [],
    }

    _check = _make_checker(report)

    # Content presence
    has_content = bool(spec.blocks) or bool(spec.sheets)
    _check("content.present" if has_content else "content.empty",
           "Document has content blocks or sheets." if has_content else "Document has no blocks or sheets.",
           QualityStatus.WARN if not has_content else QualityStatus.PASS)

    # Per-block checks
    placeholder_hits: list[str] = []
    slide_titles: list[str] = []
    source_ref_count = len(spec.source_documents)
    speaker_note_count = 0
    extracted_asset_count = 0

    for block in all_blocks:
        bt = block.type
        source_ref_count += len(block.source_refs or [])
        if block.speaker_notes:
            speaker_note_count += 1

        if block.type == BlockType.HEADING and (block.level or 1) == 1 and block.text:
            slide_titles.append(block.text)

        block_text = _block_text(block)
        if _looks_like_placeholder(block_text):
            placeholder_hits.append(_shorten(block_text))

        has_extracted_trace = _has_extracted_trace(block)
        if has_extracted_trace:
            extracted_asset_count += 1

        if bt == BlockType.TABLE:
            report["table_count"] += 1
            if has_extracted_trace:
                report["source_extracted_table_count"] += 1
            headers_len = len(block.headers or [])
            rows_len = len(block.rows or [])
            if headers_len == 0 and rows_len == 0:
                _check("table.empty", "A table has no headers or rows.", QualityStatus.WARN)
            elif rows_len > 0:
                for i, row in enumerate(block.rows or []):
                    if len(row) != headers_len:
                        _check("table.irregular", f"Table row {i} has {len(row)} columns, expected {headers_len}.", QualityStatus.WARN)

        elif bt == BlockType.FORMULA:
            report["formula_count"] += 1
            report["latex_only_formula_count"] += 1
            if has_extracted_trace:
                report["source_extracted_formula_count"] += 1
            if not block.latex:
                _check("formula.empty", "A formula block has no latex content.", QualityStatus.WARN)

        elif bt == BlockType.CHART:
            report["chart_count"] += 1
            if _has_source_ref(block):
                report["source_grounded_chart_count"] += 1
            labels_len = len(block.labels or [])
            if labels_len == 0 and not block.series:
                _check("chart.empty", "A chart has no labels or series.", QualityStatus.WARN)
            if block.series and labels_len > 0:
                for s in block.series:
                    if len(s.values) != labels_len:
                        _check("chart.irregular", f"Chart series '{s.name}' has {len(s.values)} values but {labels_len} labels.", QualityStatus.WARN)

        elif bt == BlockType.IMAGE:
            report["image_count"] += 1
            role = _asset_role(block)
            if role in ("formula", "formula_image", "formula_candidate") and has_extracted_trace:
                report["formula_image_count"] += 1
                report["source_extracted_formula_count"] += 1
            if role in ("figure", "source_figure", "diagram", "plot") and has_extracted_trace:
                report["source_extracted_figure_count"] += 1
            if role in ("table", "table_image", "source_table") and has_extracted_trace:
                report["source_extracted_table_count"] += 1
            if not block.path:
                _check("image.nopath", "An image block has no path.", QualityStatus.WARN)
            elif not _path_exists(block.path):
                _check("image.missing", f"Image path does not exist: {block.path}", QualityStatus.WARN)
            if block.source_path and block.crop_box and len(block.crop_box) not in (4,):
                _check("image.crop.invalid", "An image crop_box must contain four numbers.", QualityStatus.WARN)

    report["source_ref_count"] = source_ref_count
    report["speaker_note_count"] = speaker_note_count
    report["extracted_asset_count"] = extracted_asset_count

    _apply_asset_manifest_report(report, extra, _check)

    if format == "pptx":
        _apply_pptx_structure_report(report, extra, _check)

    if placeholder_hits:
        _check(
            "content.placeholders",
            f"Placeholder-like text remains in generated content: {', '.join(placeholder_hits[:5])}.",
            QualityStatus.WARN,
        )

    if len(slide_titles) != len(set(slide_titles)):
        _check("pptx.duplicate_titles", "PPTX contains duplicate slide titles.", QualityStatus.WARN)

    # Sheet checks (XLSX)
    for sheet in spec.sheets:
        if not sheet.rows and not sheet.formulas:
            _check("sheet.empty", f"Sheet '{sheet.name}' has no rows or formulas.", QualityStatus.WARN)

    _check_generation_contract(format, spec, report, _check, slide_titles, extra)

    # Format-specific notes
    if format in ("pptx",):
        slide_count = len([b for b in spec.blocks if b.type == BlockType.HEADING and (b.level or 1) == 1])
        if slide_count == 0:
            slide_count = 1  # implicit slide from non-heading content
        if slide_count > 80:
            _check("pptx.too_many_slides", f"PPTX has {slide_count} slides (max recommended: 80).", QualityStatus.WARN)
        # extra key for convenience
        report["slide_count"] = extra.get("slide_count", slide_count)

    if format in ("xlsx",):
        sheet_count = len(spec.sheets)
        if sheet_count == 0 and report["table_count"] == 0:
            _check("xlsx.empty", "XLSX has no sheets or tables.", QualityStatus.WARN)

    # Format warning
    formula_evidence_count = report["formula_count"] + report.get("formula_image_count", 0)

    if format in ("docx", "pptx", "pdf"):
        if report["formula_count"] > 0:
            report["rendered_formula_count"] = report["formula_count"]
            report["latex_only_formula_count"] = 0
            editable = int(report.get("editable_formula_count", 0) or 0)
            if format == "pptx" and editable >= report["formula_count"]:
                _check("formula.rendering", f"All formula blocks in {format.upper()} are rendered as editable Office Math objects.", QualityStatus.PASS)
            elif format == "pptx" and editable > 0:
                _check("formula.rendering", f"{editable} formula block(s) in {format.upper()} are editable Office Math; remaining formula(s) use image fallback.", QualityStatus.WARN)
            else:
                _check("formula.rendering", f"Formulas in {format.upper()} are rendered as high-resolution images.", QualityStatus.PASS)
        if report.get("formula_image_count", 0) > 0:
            _check("formula.image_evidence", f"{report['formula_image_count']} source formula image(s) are embedded.", QualityStatus.PASS)
        if report["chart_count"] > 0:
            _check("chart.rendering", f"Charts in {format.upper()} are rendered as embedded chart objects or images.", QualityStatus.PASS)

    contract = spec.generation_contract
    if contract and contract.minimum_quality_score is not None:
        projected_score = max(0, min(100, 100 - (report["warning_count"] * 3 + report["failure_count"] * 18 + report["blocker_count"] * 30)))
        if projected_score < contract.minimum_quality_score:
            _contract_check(_check, contract, "quality.score_below_minimum", f"Projected quality score {projected_score} is below the required minimum {contract.minimum_quality_score}.")

    _finish_report(report)
    return report


PLACEHOLDER_RE = re.compile(
    r"(\bTBD\b|\bTODO\b|\bXXX\b|\bunknown\b|\blorem ipsum\b|\binsert\b|"
    r"\[.*?(title|paper|author|figure|table|name).*?\]|Figure\s+X|Table\s+X)",
    re.IGNORECASE,
)


def _iter_blocks(blocks: list[SpecBlock]):
    for block in blocks:
        yield block
        if block.left:
            yield from _iter_blocks(block.left)
        if block.right:
            yield from _iter_blocks(block.right)


def _block_text(block: SpecBlock) -> str:
    parts: list[str] = []
    for value in (
        block.text,
        block.caption,
        block.title,
        block.display,
        block.latex,
        block.code,
        block.key_message,
        block.speaker_notes,
    ):
        if value:
            parts.append(str(value))
    if block.items:
        parts.extend(str(i) for i in block.items)
    if block.headers:
        parts.extend(str(h) for h in block.headers)
    if block.rows:
        for row in block.rows:
            parts.extend(str(cell) for cell in row)
    return " ".join(parts)


def _looks_like_placeholder(text: str) -> bool:
    return bool(text and PLACEHOLDER_RE.search(text))


def _shorten(text: str, limit: int = 80) -> str:
    collapsed = re.sub(r"\s+", " ", text).strip()
    return collapsed if len(collapsed) <= limit else collapsed[: limit - 1] + "..."


def _path_exists(path: str) -> bool:
    return os.path.exists(path) or os.path.exists(os.path.join(os.getcwd(), path))


def _asset_role(block: SpecBlock) -> str:
    metadata = block.metadata or {}
    return str(block.asset_role or metadata.get("asset_role") or metadata.get("assetRole") or "").strip().lower()


def _has_source_ref(block: SpecBlock) -> bool:
    for ref in block.source_refs or []:
        if any((
            ref.document,
            ref.page is not None,
            ref.section,
            ref.figure,
            ref.table,
            ref.equation,
            ref.quote,
            ref.asset_path,
        )):
            return True
    return False


def _has_extracted_trace(block: SpecBlock) -> bool:
    """Return true only when a block is traceable to a source crop/extracted asset.

    A plain `asset_role` is not enough: generated placeholders often carry a
    role, but source-backed academic decks need page/crop evidence or a manifest
    source image path.
    """

    has_source = bool(str(block.source_path or "").strip())
    has_crop = isinstance(block.crop_box, list) and len(block.crop_box) == 4
    if has_source and has_crop:
        return True
    metadata = block.metadata or {}
    if metadata.get("source_image_path") and (has_source or has_crop or _has_source_ref(block)):
        return True
    if metadata.get("extraction_method") and (has_source or has_crop):
        return True
    return False


def _apply_asset_manifest_report(report: dict[str, Any], extra: dict[str, Any], check) -> None:
    manifest = extra.get("asset_manifest")
    if not isinstance(manifest, dict):
        return

    report["asset_manifest_checked"] = True
    complete = manifest.get("complete")
    report["asset_manifest_complete"] = bool(complete) if isinstance(complete, bool) else None

    assets = [asset for asset in manifest.get("assets") or [] if isinstance(asset, dict)]
    report["asset_manifest_asset_count"] = int(manifest.get("asset_count") or len(assets) or 0)
    if not assets:
        assets = _manifest_assets_from_suggestions(manifest)

    counts = {"figure": 0, "table": 0, "formula": 0}
    for asset in assets:
        role = str(asset.get("role") or asset.get("asset_role") or asset.get("assetRole") or "").lower()
        if role in ("figure", "source_figure", "diagram", "plot"):
            counts["figure"] += 1
        elif role in ("table", "table_image", "source_table"):
            counts["table"] += 1
        elif role in ("formula", "formula_image", "formula_candidate"):
            counts["formula"] += 1

    report["asset_manifest_figure_count"] = counts["figure"]
    report["asset_manifest_table_count"] = counts["table"]
    report["asset_manifest_formula_count"] = counts["formula"]

    if report["asset_manifest_asset_count"] > 0:
        check("assets.manifest_present", f"Asset manifest records {report['asset_manifest_asset_count']} extracted asset(s).", QualityStatus.PASS)
    else:
        check("assets.manifest_empty", "Asset manifest is present but contains no extracted assets.", QualityStatus.WARN)


def _manifest_assets_from_suggestions(manifest: dict[str, Any]) -> list[dict[str, Any]]:
    assets: list[dict[str, Any]] = []
    for block in manifest.get("suggested_image_blocks") or []:
        if isinstance(block, dict):
            assets.append({"role": block.get("asset_role") or block.get("assetRole") or "figure"})
    for block in manifest.get("suggested_table_blocks") or []:
        if isinstance(block, dict):
            assets.append({"role": block.get("asset_role") or block.get("assetRole") or "table_image"})
    for block in manifest.get("suggested_formula_blocks") or []:
        if isinstance(block, dict):
            assets.append({"role": block.get("asset_role") or block.get("assetRole") or "formula_candidate"})
    return assets


def _apply_pptx_structure_report(report: dict[str, Any], extra: dict[str, Any], check) -> None:
    report["pptx_structure_checked"] = bool(extra.get("pptx_structure_checked"))
    for key in (
        "embedded_image_count",
        "rendered_table_count",
        "rendered_chart_count",
        "editable_formula_count",
        "pptx_picture_count",
        "pptx_media_file_count",
        "pptx_table_object_count",
        "pptx_chart_object_count",
        "pptx_office_math_count",
        "pptx_formula_image_count",
    ):
        if key in extra:
            try:
                report[key] = int(extra.get(key) or 0)
            except (TypeError, ValueError):
                report[key] = 0

    if extra.get("pptx_structure_error"):
        check("pptx.structure_inspect_failed", f"Could not inspect generated PPTX structure: {extra['pptx_structure_error']}", QualityStatus.WARN)
        return
    if not report["pptx_structure_checked"]:
        return

    if report["image_count"] > 0:
        if report["embedded_image_count"] >= report["image_count"]:
            check("pptx.images_embedded", f"{report['embedded_image_count']} image block(s) are embedded as PowerPoint pictures.", QualityStatus.PASS)
        else:
            check("pptx.images_missing", f"Spec contains {report['image_count']} image block(s), but generated PPTX contains {report['embedded_image_count']} embedded image picture(s).", QualityStatus.WARN)

    if report["table_count"] > 0:
        if report["rendered_table_count"] >= report["table_count"]:
            check("pptx.tables_rendered", f"{report['rendered_table_count']} table block(s) are rendered as PowerPoint table objects.", QualityStatus.PASS)
        else:
            check("pptx.tables_missing", f"Spec contains {report['table_count']} table block(s), but generated PPTX contains {report['rendered_table_count']} table object(s).", QualityStatus.WARN)

    if report["chart_count"] > 0:
        if report["rendered_chart_count"] >= report["chart_count"]:
            check("pptx.charts_rendered", f"{report['rendered_chart_count']} chart block(s) are rendered as PowerPoint chart objects.", QualityStatus.PASS)
        else:
            check("pptx.charts_missing", f"Spec contains {report['chart_count']} chart block(s), but generated PPTX contains {report['rendered_chart_count']} chart object(s).", QualityStatus.WARN)


def _check_generation_contract(
    format: str,
    spec: DocumentSpec,
    report: dict[str, Any],
    check,
    slide_titles: list[str],
    extra: dict[str, Any],
) -> None:
    contract = spec.generation_contract
    doc_type = spec.document_type
    is_academic = doc_type in (
        DocumentType.ACADEMIC_PAPER,
        DocumentType.DEGREE_DEFENSE,
        DocumentType.ACADEMIC_TALK,
    )

    if spec.source_documents:
        check("sources.present", f"{len(spec.source_documents)} source document reference(s) recorded.", QualityStatus.PASS)
    elif is_academic or (contract and contract.strict_source_grounding):
        _contract_check(check, contract, "sources.missing", "Academic or grounded document has no source_documents recorded.")

    if contract and contract.strict_source_grounding and report.get("source_ref_count", 0) == 0:
        _contract_check(check, contract, "sources.block_refs_missing", "Strict source grounding is enabled but no block-level source_refs were recorded.")

    if contract and contract.required_assets:
        req = contract.required_assets
        actual_images = report.get("embedded_image_count", 0) if report.get("pptx_structure_checked") else report["image_count"]
        actual_tables = report.get("rendered_table_count", 0) if report.get("pptx_structure_checked") else report["table_count"]
        actual_charts = report.get("rendered_chart_count", 0) if report.get("pptx_structure_checked") else report["chart_count"]
        if actual_images < req.figures:
            _contract_check(check, contract, "assets.figures_missing", f"Expected at least {req.figures} embedded figure image(s), found {actual_images}.")
        if actual_tables < req.tables:
            _contract_check(check, contract, "assets.tables_missing", f"Expected at least {req.tables} rendered table(s), found {actual_tables}.")
        formula_evidence_count = report["formula_count"] + report.get("formula_image_count", 0)
        if formula_evidence_count < req.formulas:
            _contract_check(check, contract, "assets.formulas_missing", f"Expected at least {req.formulas} formula evidence item(s), found {formula_evidence_count}.")
        if actual_charts < req.charts:
            _contract_check(check, contract, "assets.charts_missing", f"Expected at least {req.charts} rendered chart(s), found {actual_charts}.")
        if req.require_source_refs and report.get("source_ref_count", 0) == 0:
            _contract_check(check, contract, "assets.source_refs_missing", "Required assets need source references, but none were recorded.")
        if req.require_extracted_assets and report.get("extracted_asset_count", 0) == 0:
            _contract_check(check, contract, "assets.extracted_missing", "Required source-extracted assets were requested, but no block records a source crop or extracted asset trace.")
        if req.require_extracted_assets:
            if req.figures and report.get("source_extracted_figure_count", 0) < req.figures:
                _contract_check(check, contract, "assets.figures_untraced", f"Expected at least {req.figures} source-cropped figure image(s), found {report.get('source_extracted_figure_count', 0)}.")
            table_evidence = report.get("source_extracted_table_count", 0)
            if req.tables and table_evidence < req.tables:
                _contract_check(check, contract, "assets.tables_untraced", f"Expected at least {req.tables} source-extracted table(s), found {table_evidence}.")
            formula_evidence = report.get("source_extracted_formula_count", 0)
            if req.formulas and formula_evidence < req.formulas:
                _contract_check(check, contract, "assets.formulas_untraced", f"Expected at least {req.formulas} source-extracted formula(s), found {formula_evidence}.")

    manifest_required = bool(
        spec.source_documents
        and contract
        and contract.require_manifest
        and (is_academic or contract.strict_source_grounding or (
            contract.required_assets is not None
            and contract.required_assets.require_extracted_assets
        ))
    )
    if manifest_required:
        if not report.get("asset_manifest_checked"):
            _contract_check(check, contract, "assets.manifest_missing", "Source-backed academic document requires an extraction asset manifest, but none was provided to the renderer.")
        elif report.get("asset_manifest_complete") is False:
            _contract_check(check, contract, "assets.manifest_incomplete", "Extraction asset manifest is incomplete; document quality cannot be marked final until the source scan completes.")

        if contract and contract.required_assets and contract.required_assets.require_extracted_assets:
            req = contract.required_assets
            if req.figures and report.get("asset_manifest_figure_count", 0) < req.figures:
                _contract_check(check, contract, "assets.manifest_figures_missing", f"Asset manifest contains {report.get('asset_manifest_figure_count', 0)} figure asset(s), below required {req.figures}.")
            if req.tables and report.get("asset_manifest_table_count", 0) < req.tables:
                _contract_check(check, contract, "assets.manifest_tables_missing", f"Asset manifest contains {report.get('asset_manifest_table_count', 0)} table asset(s), below required {req.tables}.")
            if req.formulas and report.get("asset_manifest_formula_count", 0) < req.formulas:
                _contract_check(check, contract, "assets.manifest_formulas_missing", f"Asset manifest contains {report.get('asset_manifest_formula_count', 0)} formula asset(s), below required {req.formulas}.")

    if contract and contract.required_sections:
        joined_titles = " ".join(slide_titles).lower()
        missing = [s for s in contract.required_sections if s.lower() not in joined_titles]
        if missing:
            _contract_check(check, contract, "sections.missing", f"Missing required section title(s): {', '.join(missing)}.")

    expected_slide_count = contract.expected_slide_count if contract else None
    actual_slide_count = int(extra.get("slide_count") or len(slide_titles) or 0)
    if format == "pptx" and expected_slide_count:
        lower = max(1, int(expected_slide_count * 0.75))
        upper = max(expected_slide_count + 2, int(expected_slide_count * 1.35))
        if actual_slide_count < lower or actual_slide_count > upper:
            _contract_check(check, contract, "pptx.slide_count_contract", f"PPTX has {actual_slide_count} slide(s), outside expected range for {expected_slide_count}.")

    if format == "pptx" and doc_type == DocumentType.DEGREE_DEFENSE:
        if actual_slide_count < 8 and (not expected_slide_count or expected_slide_count >= 8):
            if spec.source_documents or (contract and contract.strict_source_grounding):
                check("defense.too_short", "Degree defense deck is unusually short; expected a complete research narrative.", QualityStatus.FAIL)
            else:
                check("defense.too_short", "Degree defense deck is unusually short; expected a complete research narrative.", QualityStatus.WARN)
        formula_evidence_count = report["formula_count"] + report.get("formula_image_count", 0)
        if formula_evidence_count == 0:
            if spec.source_documents or (contract and contract.strict_source_grounding):
                check("defense.no_formulas", "Degree defense deck has no formulas recorded; STEM thesis defenses should include key equations or formula evidence from the source.", QualityStatus.FAIL)
            else:
                check("defense.no_formulas", "Degree defense deck has no formulas recorded.", QualityStatus.WARN)
        if report["table_count"] == 0 and report["chart_count"] == 0 and report["image_count"] == 0:
            check("defense.no_visual_evidence", "Degree defense deck has no table/chart/image evidence blocks.", QualityStatus.WARN)
        if report["image_count"] == 0:
            if spec.source_documents:
                check("defense.no_source_figures", "Degree defense deck has no image blocks; important figures from the source PDF are missing.", QualityStatus.FAIL)
            elif contract and (contract.strict_source_grounding or (contract.required_assets and (contract.required_assets.figures > 0 or contract.required_assets.require_extracted_assets))):
                _contract_check(check, contract, "defense.no_source_figures", "Degree defense deck has no image blocks; important figures from the source PDF may be missing.")
            else:
                check("defense.no_source_figures", "Degree defense deck has no image blocks; important figures from the source PDF may be missing.", QualityStatus.WARN)
        elif (spec.source_documents or (contract and contract.strict_source_grounding)) and report.get("extracted_asset_count", 0) == 0:
            check("defense.no_extracted_source_assets", "Degree defense deck has visual blocks, but none are traced to extracted source crops or manifest-backed assets.", QualityStatus.FAIL)

    if contract and contract.require_speaker_notes and report.get("speaker_note_count", 0) == 0:
        _contract_check(check, contract, "speaker_notes.missing", "Generation contract requires speaker notes, but none were recorded.")

    if (
        contract
        and contract.require_editable_formulas
        and report["formula_count"] > 0
        and format in ("pptx", "docx", "pdf")
    ):
        editable = int(report.get("editable_formula_count", 0) or 0)
        if format == "pptx" and editable >= report["formula_count"]:
            check("formula.editable_native", f"{editable} formula block(s) are editable native Office Math equation objects.", QualityStatus.PASS)
        else:
            _contract_check(
                check,
                contract,
                "formula.editable_native_unavailable",
                f"Editable formula output was required, but only {editable} of {report['formula_count']} formula block(s) were verified as native Office Math equation objects.",
            )


def _contract_check(check, contract, id: str, message: str) -> None:
    if contract and contract.fail_on_contract_violation and not contract.allow_degraded_output:
        check(id, message, QualityStatus.FAIL)
    else:
        check(id, message, QualityStatus.WARN)


def _finish_report(report: dict[str, Any]) -> None:
    penalty = report.get("warning_count", 0) * 3 + report.get("failure_count", 0) * 18 + report.get("blocker_count", 0) * 30
    report["quality_score"] = max(0, min(100, 100 - penalty))
    if report.get("blocker_count", 0):
        report["quality_level"] = "blocked"
    elif report.get("failure_count", 0):
        report["quality_level"] = "failed"
    elif report.get("warning_count", 0):
        report["quality_level"] = "degraded"
    else:
        report["quality_level"] = "final"


def _make_checker(report: dict[str, Any]):
    """Return a closure that appends a check and increments counters."""
    def check(id: str, message: str, status: str):
        report["check_count"] += 1
        if status == QualityStatus.WARN:
            report["warning_count"] += 1
            report["warnings"].append(f"[{id}] {message}")
        elif status == QualityStatus.FAIL:
            report["failure_count"] += 1
            report["failures"].append(f"[{id}] {message}")
        elif status == QualityStatus.BLOCKER:
            report["blocker_count"] += 1
            report["blockers"].append(f"[{id}] {message}")
        report["checks"].append({"id": id, "status": status, "message": message})
    return check
