"""Tool registration — maps MCP tool names to handler functions.

Each entry in TOOL_REGISTRY maps the tool's MCP name to an async handler.
TOOL_SCHEMAS provides the tool description + inputSchema for tools/list.
"""

from __future__ import annotations

from .pptx_gen import generate_pptx, PPTX_GEN_TOOL
from .docx_gen import generate_docx, DOCX_GEN_TOOL
from .xlsx_gen import generate_xlsx, XLSX_GEN_TOOL
from .pdf_gen import generate_pdf, PDF_GEN_TOOL
from .chart_image import generate_chart_image, CHART_IMAGE_TOOL
from .formula_image import handle_formula_image, FORMULA_IMAGE_TOOL
from .extract import (
    EXTRACT_ASSETS_TOOL,
    EXTRACT_TOOL,
    extract_document_assets,
    extract_document_text,
)


# handler registry
TOOL_REGISTRY: dict[str, callable] = {
    "generate_pptx": generate_pptx,
    "generate_docx": generate_docx,
    "generate_xlsx": generate_xlsx,
    "generate_pdf": generate_pdf,
    "generate_chart_image": generate_chart_image,
    "render_formula_image": handle_formula_image,
    "extract_document_text": extract_document_text,
    "extract_document_assets": extract_document_assets,
}

# schema registry (for tools/list)
TOOL_SCHEMAS: dict[str, dict] = {
    "generate_pptx": PPTX_GEN_TOOL,
    "generate_docx": DOCX_GEN_TOOL,
    "generate_xlsx": XLSX_GEN_TOOL,
    "generate_pdf": PDF_GEN_TOOL,
    "generate_chart_image": CHART_IMAGE_TOOL,
    "render_formula_image": FORMULA_IMAGE_TOOL,
    "extract_document_text": EXTRACT_TOOL,
    "extract_document_assets": EXTRACT_ASSETS_TOOL,
}
