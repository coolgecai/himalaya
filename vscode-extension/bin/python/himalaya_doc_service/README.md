# Himalaya Document Service

High-quality document generation MCP server for the Himalaya agent system. Provides Python-backed generation of PPTX, DOCX, XLSX, and PDF documents with real charts, embedded images, editable PPTX Office Math formulas, color themes, and professional formatting.

## Installation

```bash
# From source
cd python/himalaya_doc_service
pip install -e .

# Or install dependencies directly
pip install python-pptx python-docx openpyxl reportlab matplotlib Pillow pydantic
```

## Configuration

Add to your `.Himalaya/settings.json`:

```json
{
  "mcpServers": {
    "doc-service": {
      "command": "python3",
      "args": ["-m", "himalaya_doc_service"]
    }
  }
}
```

Optionally set a longer timeout for large documents (default 60s):

```json
{
  "mcpServers": {
    "doc-service": {
      "command": "python3",
      "args": ["-m", "himalaya_doc_service"],
      "toolCallTimeoutMs": 300000
    }
  }
}
```

## Available Tools

| Tool | Description |
|------|-------------|
| `generate_pptx` | Professional PPTX with real charts, embedded images, editable tables, Office Math formulas, themes |
| `generate_docx` | Professional DOCX with styles, embedded images, tables |
| `generate_xlsx` | Professional XLSX with formulas, real charts, formatting |
| `generate_pdf` | Printable PDF with CJK text, images, tables |
| `generate_chart_image` | Standalone chart image (PNG/SVG) |
| `render_formula_image` | LaTeX formula rendered to PNG for fallback or non-PPTX use |
| `extract_document_text` | Extract text from existing documents |
| `extract_document_assets` | Resumable PDF figure/table/formula extraction with source crop metadata |

For large thesis PDFs, call `extract_document_assets` in batches instead of scanning the whole PDF at once. By default it scans a small page batch, writes `assets.manifest.json`, and returns `next_start_page`/`next_call` so a weak local model can continue deterministically. It also crops figure candidates above captions such as `图3.2`/`Fig. 3.2`, which catches vector PDF plots that are not embedded as standalone images.

For source-backed academic decks, pass the returned manifest into `generate_pptx`
as `asset_manifest_path` or `asset_manifest`. When `generation_contract.strict_source_grounding`
or `required_assets.require_extracted_assets` is enabled, missing or incomplete
manifests are treated as quality contract violations. Synthetic tables, charts,
or generated placeholder images no longer satisfy extracted-asset requirements
unless the block carries source crop evidence such as `source_path` + `crop_box`
or comes from the extraction manifest.

## Themes

Six predefined color themes are available: `default`, `ocean`, `forest`, `sunset`, `corporate`, `minimal`.

## Testing

```bash
# Run comprehensive tests
python3 tests/test_comprehensive.py

# Test MCP server interactively
echo '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}' | python3 -m himalaya_doc_service
```

## DocumentSpec Format

The DocumentSpec is a JSON schema compatible with the Rust `file-generate` crate. The Python renderer also accepts optional professional metadata for academic and long-form document tasks:

- `document_type`: `general`, `academic_paper`, `degree_defense`, `academic_talk`, or `technical_report`
- `source_documents`: source references for papers, PDFs, extracted figures, tables, or equations
- `generation_contract`: expected slide count, required sections, required assets, strict source grounding, and speaker-note requirements
- Block-level `source_refs`, `key_message`, `speaker_notes`, `layout`, `position`, `equation_number`, `source_path`, `crop_box`, `crop_units`, and `asset_role`

Quality reports surface these fields as checks for placeholder text, missing source grounding, missing figures/tables/formulas/charts, missing speaker notes, incomplete extraction manifests, untraced source assets, and too-short degree-defense decks.

It supports the following block types:

- `heading` — section title with `level` (1-6)
- `paragraph` — body text
- `bullets` — bullet list items
- `table` — table with caption, headers, rows
- `formula` — LaTeX formula rendered as editable Office Math in PPTX, with high-resolution PNG fallback for complex cases and non-PPTX outputs
- `chart` — chart with title, kind, labels, series
- `image` — embedded image from file path
- `two_column` — left/right column layout
- `quote` — block quote with attribution
- `code` — code block with language tag
- `page_break` — explicit slide/page break

For Excel output, use `sheets` array with rows, formulas (A1 cell references), and embedded charts.
