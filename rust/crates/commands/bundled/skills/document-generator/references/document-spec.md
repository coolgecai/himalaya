# DocumentSpec Reference

Use `document_spec` with the `generate_file` tool for professional documents.

## Top-Level Schema

```json
{
  "title": "string",
  "subtitle": "string",
  "author": "string",
  "language": "zh-CN",
  "theme": {
    "name": "string",
    "accentColor": "#2563EB",
    "fontFamily": "string"
  },
  "blocks": [],
  "sheets": []
}
```

All fields are optional except the fields required by each block type.

## Block Types

```json
{"type": "heading", "level": 1, "text": "Executive Summary"}
{"type": "paragraph", "text": "Main narrative text."}
{"type": "bullets", "items": ["Point one", "Point two"]}
```

```json
{
  "type": "table",
  "caption": "Quarterly metrics",
  "headers": ["Metric", "Q1", "Q2"],
  "rows": [["Revenue", "120", "145"], ["Margin", "32%", "35%"]]
}
```

```json
{
  "type": "formula",
  "latex": "NPV=\\sum_{t=1}^{n}\\frac{CF_t}{(1+r)^t}-C_0",
  "display": "NPV = sum of discounted cash flows minus initial cost"
}
```

```json
{
  "type": "chart",
  "title": "Revenue Trend",
  "kind": "bar",
  "labels": ["Q1", "Q2", "Q3"],
  "series": [
    {"name": "Revenue", "values": ["120", "145", "168"]},
    {"name": "Cost", "values": ["80", "92", "101"]}
  ]
}
```

```json
{
  "type": "image",
  "path": "assets/revenue-trend.png",
  "alt": "Revenue and cost trend chart",
  "caption": "Figure 1. Revenue trend"
}
```

## Sheet Schema

```json
{
  "name": "Model",
  "rows": [
    ["Metric", "Value"],
    ["Revenue", "120"],
    ["Cost", "80"]
  ],
  "formulas": [
    {"cell": "B4", "formula": "B2-B3", "value": "40"}
  ],
  "charts": []
}
```

Use plain Excel formula text without a leading `=` in `formulas[].formula`.

## Report Example

```json
{
  "title": "Investment Analysis",
  "subtitle": "Scenario model with formulas and charts",
  "blocks": [
    {"type": "heading", "level": 2, "text": "Highlights"},
    {"type": "bullets", "items": ["Revenue grows 18%", "Payback period improves"]},
    {
      "type": "table",
      "caption": "Base case",
      "headers": ["Metric", "Value"],
      "rows": [["Revenue", "120"], ["Cost", "80"], ["Profit", "40"]]
    },
    {"type": "formula", "latex": "Profit=Revenue-Cost"},
    {
      "type": "chart",
      "title": "Revenue",
      "labels": ["Q1", "Q2"],
      "series": [{"name": "Revenue", "values": ["120", "145"]}]
    }
  ]
}
```

## Deck Example

Use H1 heading blocks to start new PPTX slides.

```json
{
  "blocks": [
    {"type": "heading", "level": 1, "text": "Market Overview"},
    {"type": "bullets", "items": ["Demand is rising", "Competition is fragmented"]},
    {"type": "heading", "level": 1, "text": "Financial Model"},
    {"type": "chart", "title": "ARR", "labels": ["2026", "2027"], "series": [{"name": "ARR", "values": ["5", "9"]}]}
  ]
}
```

## Workbook Example

```json
{
  "title": "Financial Workbook",
  "sheets": [
    {
      "name": "Summary",
      "rows": [["Metric", "Value"], ["Revenue", "120"], ["Cost", "80"]],
      "formulas": [{"cell": "B4", "formula": "B2-B3", "value": "40"}]
    }
  ],
  "blocks": [
    {"type": "chart", "title": "Revenue", "labels": ["Q1", "Q2"], "series": [{"name": "Revenue", "values": ["120", "145"]}]}
  ]
}
```
