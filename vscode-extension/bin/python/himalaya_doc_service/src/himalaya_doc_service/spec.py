"""Structured DocumentSpec model (Pydantic).

Compatible with the Rust file-generate crate's DocumentSpec schema.
Extended with additional block types for the Python high-quality renderer.
"""

from __future__ import annotations

import re
from enum import Enum
from typing import Any, Optional

from pydantic import BaseModel, Field, field_validator, model_validator


# ---------------------------------------------------------------------------
# Enums
# ---------------------------------------------------------------------------

class BlockType(str, Enum):
    HEADING = "heading"
    PARAGRAPH = "paragraph"
    BULLETS = "bullets"
    TABLE = "table"
    FORMULA = "formula"
    CHART = "chart"
    IMAGE = "image"
    TWO_COLUMN = "two_column"
    QUOTE = "quote"
    CODE = "code"
    PAGE_BREAK = "page_break"


class ChartKind(str, Enum):
    BAR = "bar"
    LINE = "line"
    PIE = "pie"
    SCATTER = "scatter"
    AREA = "area"
    RADAR = "radar"


class ImageKind(str, Enum):
    PNG = "png"
    JPG = "jpg"
    JPEG = "jpeg"
    SVG = "svg"
    GIF = "gif"


class DocumentType(str, Enum):
    GENERAL = "general"
    ACADEMIC_PAPER = "academic_paper"
    DEGREE_DEFENSE = "degree_defense"
    ACADEMIC_TALK = "academic_talk"
    TECHNICAL_REPORT = "technical_report"


# ---------------------------------------------------------------------------
# Sub-models
# ---------------------------------------------------------------------------

class ChartSeries(BaseModel):
    name: str = ""
    values: list[float | str] = Field(default_factory=list)


class FormulaCell(BaseModel):
    cell: str  # e.g. "B3"
    formula: str  # e.g. "=SUM(B2:B2)"
    value: Optional[str] = None


class DocumentTheme(BaseModel):
    name: Optional[str] = "default"
    accent_color: Optional[str] = None
    font_family: Optional[str] = None
    background_color: Optional[str] = None


class SourceRef(BaseModel):
    """Trace a generated element back to source material.

    These fields are intentionally optional so agents can record the strongest
    evidence available: a PDF page, a section title, a paragraph range, a table
    number, a figure number, or a crop/image asset path.
    """

    document: Optional[str] = None
    page: Optional[int] = None
    section: Optional[str] = None
    paragraph: Optional[str] = None
    figure: Optional[str] = None
    table: Optional[str] = None
    equation: Optional[str] = None
    quote: Optional[str] = None
    asset_path: Optional[str] = None


class LegacySlideSpec(BaseModel):
    """Compatibility shape for model-produced slide specs.

    The canonical DocumentSpec uses `blocks`; this legacy/loose shape is
    normalized into blocks when no canonical blocks are present.
    """

    layout: Optional[str] = None
    title: Optional[str] = None
    subtitle: Optional[str] = None
    meta: Optional[str] = None
    items: list[str] = Field(default_factory=list)
    content: list[str] = Field(default_factory=list)
    speaker_notes: Optional[str] = None
    speakerNotes: Optional[str] = None
    notes: Optional[str] = None

    @field_validator("items", "content", mode="before")
    @classmethod
    def _coerce_string_list(cls, value):
        if value is None:
            return []
        if isinstance(value, str):
            return [value]
        if isinstance(value, list):
            return [str(item) for item in value if item is not None]
        return [str(value)]


class LayoutBox(BaseModel):
    """Optional placement hint for renderers that support manual layout."""

    x: Optional[float] = None
    y: Optional[float] = None
    w: Optional[float] = None
    h: Optional[float] = None
    units: str = "fraction"  # fraction, inches, or px


class AssetPlan(BaseModel):
    """Expected visual assets for long-form professional document tasks."""

    figures: int = 0
    tables: int = 0
    formulas: int = 0
    charts: int = 0
    require_source_refs: bool = False
    require_extracted_assets: bool = False


class GenerationContract(BaseModel):
    """Machine-checkable execution contract for long-running document tasks."""

    purpose: Optional[str] = None
    audience: Optional[str] = None
    expected_slide_count: Optional[int] = None
    language: Optional[str] = None
    strict_source_grounding: bool = False
    allow_degraded_output: bool = False
    fail_on_contract_violation: bool = True
    max_quality_retries: Optional[int] = None
    minimum_quality_score: Optional[int] = None
    require_manifest: bool = True
    require_speaker_notes: bool = False
    require_editable_formulas: bool = False
    required_sections: list[str] = Field(default_factory=list)
    required_assets: Optional[AssetPlan] = None
    checkpoints: list[str] = Field(default_factory=list)


class SheetSpec(BaseModel):
    name: str = "Sheet1"
    rows: list[list[str]] = Field(default_factory=list)
    formulas: list[FormulaCell] = Field(default_factory=list)
    charts: list[SpecBlock] = Field(default_factory=list)


# ---------------------------------------------------------------------------
# SpecBlock — the core content block (forward-ref for TWO_COLUMN recursion)
# ---------------------------------------------------------------------------

class SpecBlock(BaseModel):
    type: BlockType = BlockType.PARAGRAPH

    # heading
    level: Optional[int] = Field(default=1, ge=1, le=6)
    text: Optional[str] = None

    # bullets
    items: Optional[list[str]] = None

    # table
    caption: Optional[str] = None
    headers: Optional[list[str]] = None
    rows: Optional[list[list[str]]] = None

    # formula
    latex: Optional[str] = None
    display: Optional[str] = None
    formula_format: Optional[str] = None  # auto, office_math/omml, image/png
    formulaFormat: Optional[str] = None

    # chart
    title: Optional[str] = None
    kind: Optional[ChartKind] = None
    labels: Optional[list[str]] = None
    series: Optional[list[ChartSeries]] = None

    # image
    path: Optional[str] = None
    alt: Optional[str] = None
    image_kind: Optional[ImageKind] = None
    source_path: Optional[str] = None
    crop_box: Optional[list[float]] = None
    crop_units: Optional[str] = None
    asset_role: Optional[str] = None  # figure, table_image, formula_image, icon, background

    # two_column
    left: Optional[list[SpecBlock]] = None
    right: Optional[list[SpecBlock]] = None

    # quote
    attribution: Optional[str] = None

    # code
    language: Optional[str] = None
    code: Optional[str] = None

    # professional/academic rendering and provenance
    key_message: Optional[str] = None
    speaker_notes: Optional[str] = None
    layout: Optional[str] = None
    position: Optional[LayoutBox] = None
    source_refs: list[SourceRef] = Field(default_factory=list)
    equation_number: Optional[str] = None
    metadata: dict[str, Any] = Field(default_factory=dict)


# Resolve forward reference for recursive TWO_COLUMN
SpecBlock.model_rebuild()


# ---------------------------------------------------------------------------
# DocumentSpec — top-level document specification
# ---------------------------------------------------------------------------

class DocumentSpec(BaseModel):
    title: Optional[str] = None
    subtitle: Optional[str] = None
    author: Optional[str] = None
    language: Optional[str] = "zh-CN"
    document_type: DocumentType = DocumentType.GENERAL
    audience: Optional[str] = None
    source_documents: list[SourceRef] = Field(default_factory=list)
    generation_contract: Optional[GenerationContract] = None
    metadata: dict[str, Any] = Field(default_factory=dict)
    theme: Optional[DocumentTheme] = None
    blocks: list[SpecBlock] = Field(default_factory=list)
    sheets: list[SheetSpec] = Field(default_factory=list)
    slides: list[LegacySlideSpec] = Field(default_factory=list)

    @model_validator(mode="after")
    def _normalize_legacy_slides(self):
        if self.blocks or not self.slides:
            return self
        blocks: list[SpecBlock] = []
        for idx, slide in enumerate(self.slides):
            title = (slide.title or slide.layout or f"Slide {idx + 1}").strip() or f"Slide {idx + 1}"
            blocks.append(
                SpecBlock(
                    type=BlockType.HEADING,
                    level=1,
                    text=title,
                    speaker_notes=slide.speaker_notes or slide.speakerNotes or slide.notes,
                )
            )
            for text in (slide.subtitle, slide.meta):
                if text and text.strip():
                    blocks.append(SpecBlock(type=BlockType.PARAGRAPH, text=text.strip()))
            if slide.items:
                blocks.append(SpecBlock(type=BlockType.BULLETS, items=slide.items))
            for text in slide.content:
                blocks.extend(_blocks_from_text_with_formulas(text))
        self.blocks = blocks
        return self


def _blocks_from_text_with_formulas(text: str) -> list[SpecBlock]:
    blocks: list[SpecBlock] = []
    remaining = text or ""
    while True:
        match = re.search(r"\$\$(.*?)\$\$", remaining, flags=re.S)
        if not match:
            stripped = remaining.strip()
            if stripped:
                blocks.append(SpecBlock(type=BlockType.PARAGRAPH, text=stripped))
            return blocks
        before = remaining[: match.start()].strip()
        if before:
            blocks.append(SpecBlock(type=BlockType.PARAGRAPH, text=before))
        latex = match.group(1).strip()
        if latex:
            blocks.append(SpecBlock(type=BlockType.FORMULA, latex=latex))
        remaining = remaining[match.end() :]
