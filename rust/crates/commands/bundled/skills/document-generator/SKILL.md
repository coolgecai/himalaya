---
name: document-generator
description: Create professional Word/DOCX, PowerPoint/PPTX, PDF, and Excel/XLSX documents from markdown or structured DocumentSpec JSON. Use when Codex needs to generate reports, decks, printable PDFs, spreadsheets, tables, formulas, chart data, image-backed figures, workbook sheets, or quality manifests.
---

# Document Generator

Use `generate_file` for binary document output. Prefer `document_spec` for polished or data-heavy documents; use `content` only for quick markdown drafts.

## Workflow

1. Choose the output format: `docx`, `pptx`, `pdf`, or `xlsx`.
2. If the request references attachments or source documents, read/extract them first and base the document on that evidence. Do not stop after summarizing the source.
3. Build a `document_spec` with title metadata, ordered content blocks, and optional workbook sheets.
4. Include structured blocks for tables, formulas, charts, and images instead of flattening them into prose.
5. Run `generate_file` and inspect the returned `quality` object plus `manifestPath`.
6. If quality warnings report missing images, irregular table widths, chart series mismatches, empty sheets, or non-XLSX formula/chart rendering limits, fix the spec and regenerate.
7. Final output must include the generated file path and manifest path, or clearly state the blocking error if no file was created.

## DocumentSpec

Read `references/document-spec.md` when constructing a non-trivial spec or when examples are needed.

Core block types:

- `heading`: section title with `level` and `text`.
- `paragraph`: body text.
- `bullets`: list of `items`.
- `table`: `caption`, `headers`, and `rows`.
- `formula`: preserve LaTeX in `latex`; use `display` only when a friendlier visible form is needed.
- `chart`: provide `title`, `kind`, `labels`, and named `series`.
- `image`: provide a workspace-relative or absolute `path`, plus `alt` and `caption`.

For Excel output, put editable worksheet data in `sheets[].rows` and formulas in `sheets[].formulas` with A1 cell addresses. Chart blocks are also converted into chart-data sheets.

## Output Notes

- DOCX creates editable headings, paragraphs, bullet lists, tables, chart data tables, and formula source text.
- PPTX creates editable slide XML; each H1 starts a new slide.
- PDF creates a printable document and vector bar rendering for simple chart blocks.
- XLSX creates editable workbook XML with sheets, rows, formulas, and chart backing data.
- The manifest is written next to the document as `<file>.<format>.manifest.json` and records quality checks.

## Chart Images

Use `scripts/chart_asset.py` only when a standalone high-resolution chart image is needed before document generation. It accepts a chart JSON file and writes PNG or SVG output that can be referenced by an `image` block.
