# Visuals in Zeron

Zeron draws three fenced blocks in chat. Use one only when a visual says more than a sentence would; plain text is the default.

- **Exact values to compare:** a Markdown table.
- **Steps, decisions, architecture or flows:** a ` ```mermaid ` block (flowchart, sequence, class, state or ER).
- **Numbers over categories or time:** a ` ```chart ` block holding one JSON object:

```chart
{"type": "bar", "title": "Build time by target", "unit": "s",
 "data": [{"label": "macOS", "value": 412}, {"label": "Linux", "value": 365}]}
```

Chart fields:
- `type`: `bar`, `line`, `area`, `scatter` or `pie` (a few parts of one whole).
- `data`: `[{"label", "value"}]`; scatter: `[{"x", "y", "label"?}]`.
- `series`: for several series, `[{"name", "data": [...]}]` instead of `data` (a pie takes one).
- Optional: `title`, `description`, `xLabel`, `yLabel`, `unit` (`"%"`, `"$"`, `"ms"`), `source`.
- Real numbers only; if illustrative or estimated, say so in `source`. At most 8 series and 200 points. Valid JSON: no comments or trailing commas.

## Showing a page

When these blocks cannot draw it (interaction, many panels, custom layout), build one self-contained HTML page, check it with `zeron_html_preview`, then publish it with `zeron_html_render` before your final reply. It shows live in your reply: don't announce or restate it.
