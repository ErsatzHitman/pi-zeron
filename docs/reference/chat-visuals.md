# Chat visuals

Assistant replies can carry three kinds of visuals. Zeron draws all of them itself, with no web view, in the same frame as a code block. **Show source** on that frame switches back to the fence.

| Fence | Drawn as | Interaction |
| --- | --- | --- |
| Markdown table | table | scrolls sideways when wide |
| ` ```mermaid ` | diagram (rendered off the UI thread) | click to open full size |
| ` ```chart ` | bar, line, area, scatter or pie chart | hover a mark for its value; click a legend entry to hide or show a series (or a pie slice) |

A fence that is still streaming stays as source until its block is complete. An invalid chart keeps its source and a warning that says what to fix.

## Chart JSON

The fields follow OpenIntelligentUI's chart props (`title`, `description`, `data: [{label, value}]`). Zeron adds:

- `type`: `bar` (the default), `line`, `area`, `scatter` or `pie`.
- `series`: `[{name, data}]`, for up to 8 series.
- Axis labels: `xLabel` and `yLabel`.
- `unit`, such as `%`, `$` or `ms`.
- `source`, a provenance line.

Scatter points are `{x, y, label?}`. A pie takes one series of values of zero or more. Each series can have at most 200 points.

```chart
{"type": "line", "title": "Weekly active users", "unit": "k",
 "series": [{"name": "2025", "data": [{"label": "W1", "value": 12}, {"label": "W2", "value": 15}]},
            {"name": "2026", "data": [{"label": "W1", "value": 18}, {"label": "W2", "value": 21}]}]}
```

Series take colours from the theme accent first, then a fixed palette. Pi learns the format through `crates/harness/src/pi/visuals.md`, which is appended to every Pi chat's system prompt. Other harnesses render the same fences when their agents write them.

Code: `crates/ui/src/markdown/chart.rs` (parse, layout, paint and state), wired through `MediaUi::chart` in the transcript and the Markdown file preview.
