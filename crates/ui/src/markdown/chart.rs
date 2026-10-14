//! ` ```chart ` fences drawn natively: bar, line, area, scatter and pie. The
//! JSON keeps OpenIntelligentUI's chart props (`title`, `description`,
//! `data: [{label, value}]`) and adds `type`, `series` for several series,
//! axis labels, a `unit` and a `source` line. Hovering a mark shows its value;
//! clicking a legend entry hides or shows that series (or pie slice).
use std::cell::RefCell;
use std::collections::{BTreeSet, HashMap, HashSet};
use std::rc::Rc;

use gpui::{
    AnyElement, App, Hsla, PathBuilder, SharedString, Window, canvas, div, point, prelude::*, px,
    relative,
};
use serde_json::Value;

use super::render::{DiagramUi, DiagramView};
use crate::settings::widgets::text_tooltip;
use crate::theme::Theme;

const MAX_SERIES: usize = 8;
const MAX_POINTS: usize = 200;
const PLOT_HEIGHT: f32 = 200.0;
const AXIS_WIDTH: f32 = 44.0;
const PIE_SIZE: f32 = 168.0;
/// After the theme accent: OpenIntelligentUI's chart colors.
const PALETTE: [u32; 7] = [
    0x8b5cf6, 0xec4899, 0xf59e0b, 0x10b981, 0x06b6d4, 0xf97316, 0x3b82f6,
];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Kind {
    Bar,
    Line,
    Area,
    Scatter,
    Pie,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Point {
    /// The category (bar, line, area, pie) or an optional name (scatter).
    pub label: Option<String>,
    /// Scatter only: the numeric x value.
    pub x: f64,
    pub y: f64,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Series {
    pub name: String,
    pub points: Vec<Point>,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Chart {
    pub kind: Kind,
    pub title: Option<String>,
    pub description: Option<String>,
    /// Category order across every series (empty for scatter).
    pub labels: Vec<String>,
    pub series: Vec<Series>,
    pub x_label: Option<String>,
    pub y_label: Option<String>,
    pub unit: Option<String>,
    pub source: Option<String>,
}

fn text(value: &Value, key: &str) -> Option<String> {
    value
        .get(key)?
        .as_str()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
}

fn number(value: Option<&Value>) -> Option<f64> {
    let n = match value? {
        Value::Number(n) => n.as_f64()?,
        Value::String(s) => s
            .trim()
            .trim_end_matches('%')
            .replace(',', "")
            .parse()
            .ok()?,
        _ => return None,
    };
    n.is_finite().then_some(n)
}

fn label_of(point: &Value) -> Option<String> {
    ["label", "name", "x", "category"]
        .iter()
        .find_map(|key| match point.get(*key)? {
            Value::String(s) => Some(s.trim().to_owned()),
            Value::Number(n) => Some(n.to_string()),
            _ => None,
        })
}

fn points(data: &Value, kind: Kind, series: usize) -> Result<Vec<Point>, String> {
    let Some(items) = data.as_array() else {
        return Err("`data` must be a list of points.".into());
    };
    if items.len() > MAX_POINTS {
        return Err(format!("A series can have at most {MAX_POINTS} points."));
    }
    items
        .iter()
        .enumerate()
        .map(|(ix, item)| {
            let at = || {
                if series > 0 {
                    format!("Point {} of series {}", ix + 1, series + 1)
                } else {
                    format!("Point {}", ix + 1)
                }
            };
            if kind == Kind::Scatter {
                let x = number(item.get("x"))
                    .ok_or_else(|| format!("{} needs a numeric `x`.", at()))?;
                let y = number(item.get("y").or_else(|| item.get("value")))
                    .ok_or_else(|| format!("{} needs a numeric `y`.", at()))?;
                return Ok(Point {
                    label: text(item, "label").or_else(|| text(item, "name")),
                    x,
                    y,
                });
            }
            let y = number(item.get("value").or_else(|| item.get("y")))
                .ok_or_else(|| format!("{} needs a numeric `value`.", at()))?;
            Ok(Point {
                label: Some(label_of(item).unwrap_or_else(|| (ix + 1).to_string())),
                x: ix as f64,
                y,
            })
        })
        .collect()
}

/// Read a chart fence. `Err` says what to fix, in a sentence.
pub(crate) fn parse(code: &str) -> Result<Chart, String> {
    let value: Value = serde_json::from_str(code.trim())
        .map_err(|e| format!("This chart is not valid JSON ({e})."))?;
    if !value.is_object() {
        return Err("A chart is a JSON object.".into());
    }
    let kind = match text(&value, "type")
        .or_else(|| text(&value, "kind"))
        .map(|t| t.to_ascii_lowercase())
        .as_deref()
    {
        None | Some("bar" | "column" | "bars") => Kind::Bar,
        Some("line") => Kind::Line,
        Some("area") => Kind::Area,
        Some("scatter" | "point" | "points") => Kind::Scatter,
        Some("pie" | "donut" | "doughnut") => Kind::Pie,
        Some(other) => {
            return Err(format!(
                "Unknown chart type “{other}”. Use bar, line, area, scatter or pie."
            ));
        }
    };
    let mut series = Vec::new();
    if let Some(list) = value.get("series").and_then(Value::as_array) {
        if list.len() > MAX_SERIES {
            return Err(format!("A chart can have at most {MAX_SERIES} series."));
        }
        for (ix, item) in list.iter().enumerate() {
            let data = item
                .get("data")
                .ok_or_else(|| format!("Series {} needs `data`.", ix + 1))?;
            series.push(Series {
                name: text(item, "name")
                    .or_else(|| text(item, "label"))
                    .unwrap_or_else(|| format!("Series {}", ix + 1)),
                points: points(data, kind, ix)?,
            });
        }
    } else if let Some(data) = value.get("data") {
        series.push(Series {
            name: text(&value, "name")
                .or_else(|| text(&value, "yLabel"))
                .unwrap_or_else(|| "Value".into()),
            points: points(data, kind, 0)?,
        });
    }
    if series.iter().all(|s| s.points.is_empty()) {
        return Err("A chart needs `data` or `series` with at least one point.".into());
    }
    if kind == Kind::Pie {
        if series.len() > 1 {
            return Err("A pie chart takes one series.".into());
        }
        if series[0].points.iter().any(|p| p.y < 0.0) {
            return Err("A pie chart takes values of zero or more.".into());
        }
    }
    let mut labels: Vec<String> = Vec::new();
    if kind != Kind::Scatter {
        for point in series.iter().flat_map(|s| &s.points) {
            let label = point.label.clone().unwrap_or_default();
            if !labels.contains(&label) {
                labels.push(label);
            }
        }
    }
    Ok(Chart {
        kind,
        title: text(&value, "title"),
        description: text(&value, "description"),
        labels,
        series,
        x_label: text(&value, "xLabel"),
        y_label: text(&value, "yLabel"),
        unit: text(&value, "unit"),
        source: text(&value, "source"),
    })
}

impl Chart {
    /// The value of series `s` at category `c`.
    fn at(&self, s: usize, c: usize) -> Option<f64> {
        let label = self.labels.get(c)?;
        self.series[s]
            .points
            .iter()
            .find(|p| p.label.as_ref() == Some(label))
            .map(|p| p.y)
    }
}

/// Round axis ticks spanning `lo..hi`.
pub(crate) fn ticks(lo: f64, hi: f64) -> Vec<f64> {
    let (lo, hi) = if hi - lo < f64::EPSILON {
        (lo - 1.0, hi + 1.0)
    } else {
        (lo, hi)
    };
    let raw = (hi - lo) / 4.0;
    let magnitude = 10f64.powf(raw.log10().floor());
    let step = [1.0, 2.0, 2.5, 5.0, 10.0]
        .iter()
        .map(|m| m * magnitude)
        .find(|step| *step >= raw)
        .unwrap_or(10.0 * magnitude);
    let start = (lo / step).floor() * step;
    let mut out = Vec::new();
    let mut tick = start;
    while tick < hi + step * 0.5 && out.len() < 12 {
        out.push(if tick.abs() < step * 1e-9 { 0.0 } else { tick });
        tick += step;
        if tick > hi + step * 1e-9 && out.last().is_some_and(|last| *last >= hi) {
            break;
        }
    }
    out
}

/// "1,250", "3.5k", "2.4M", "0.25", with the unit after.
pub(crate) fn format_value(value: f64, unit: Option<&str>) -> String {
    let abs = value.abs();
    let mut text = if abs >= 1e9 {
        format!("{:.1}B", value / 1e9)
    } else if abs >= 1e6 {
        format!("{:.1}M", value / 1e6)
    } else if abs >= 1e4 {
        format!("{:.1}k", value / 1e3)
    } else if value.fract().abs() < 1e-9 {
        let digits = format!("{}", value.abs() as u64);
        let mut grouped = String::new();
        for (ix, digit) in digits.chars().enumerate() {
            if ix > 0 && (digits.len() - ix) % 3 == 0 {
                grouped.push(',');
            }
            grouped.push(digit);
        }
        if value < 0.0 {
            format!("-{grouped}")
        } else {
            grouped
        }
    } else {
        let s = format!("{value:.2}");
        s.trim_end_matches('0').trim_end_matches('.').to_owned()
    };
    text = text
        .replace(".0B", "B")
        .replace(".0M", "M")
        .replace(".0k", "k");
    match unit {
        Some("%") => format!("{text}%"),
        Some(unit) if unit.chars().count() <= 2 && !unit.chars().any(char::is_alphabetic) => {
            format!("{unit}{text}")
        }
        Some(unit) => format!("{text} {unit}"),
        None => text,
    }
}

fn color(theme: &Theme, ix: usize) -> Hsla {
    if ix == 0 {
        theme.accent
    } else {
        gpui::rgb(PALETTE[(ix - 1) % PALETTE.len()]).into()
    }
}

/// Toggle a series (or pie slice) by index.
pub(crate) type Toggle = Rc<dyn Fn(usize, &mut Window, &mut App)>;

pub(crate) fn render(
    chart: &Chart,
    id: SharedString,
    hidden: &BTreeSet<usize>,
    toggle: Toggle,
    theme: &Theme,
) -> AnyElement {
    let header = (chart.title.is_some() || chart.description.is_some()).then(|| {
        div()
            .flex()
            .flex_col()
            .gap(px(2.0))
            .children(chart.title.clone().map(|title| {
                div()
                    .text_size(px(13.0))
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .text_color(theme.text)
                    .child(title)
            }))
            .children(chart.description.clone().map(|description| {
                div()
                    .text_size(px(12.0))
                    .text_color(theme.text_muted)
                    .child(description)
            }))
    });
    let body = match chart.kind {
        Kind::Pie => pie(chart, &id, hidden, toggle, theme),
        _ => cartesian(chart, &id, hidden, toggle, theme),
    };
    div()
        .w_full()
        .p(px(12.0))
        .flex()
        .flex_col()
        .gap(px(10.0))
        .children(header)
        .child(body)
        .children(chart.source.clone().map(|source| {
            div()
                .text_size(px(11.0))
                .text_color(theme.text_faint)
                .child(format!("Source: {source}"))
        }))
        .into_any_element()
}

#[allow(clippy::too_many_arguments)]
fn legend_chip(
    id: SharedString,
    name: String,
    detail: Option<String>,
    swatch: Hsla,
    hidden: bool,
    ix: usize,
    toggle: Toggle,
    theme: &Theme,
) -> AnyElement {
    let label = if hidden { "Show" } else { "Hide" };
    let selector = id.to_string();
    div()
        .id(id)
        .debug_selector(move || selector)
        .flex()
        .flex_row()
        .items_center()
        .gap(px(6.0))
        .px(px(6.0))
        .py(px(2.0))
        .rounded(px(6.0))
        .cursor_pointer()
        .text_size(px(12.0))
        .text_color(theme.text_muted)
        .hover(|s| s.bg(crate::theme::ink(0.05)))
        .when(hidden, |el| el.opacity(0.45))
        .tooltip(text_tooltip(format!("{label} {name}")))
        .on_click(move |_, window, cx| toggle(ix, window, cx))
        .child(
            div()
                .size(px(8.0))
                .flex_none()
                .rounded_full()
                .when(hidden, |el| el.border_1().border_color(swatch))
                .when(!hidden, |el| el.bg(swatch)),
        )
        .child(name)
        .children(detail.map(|detail| div().text_color(theme.text_faint).child(detail)))
        .into_any_element()
}

fn axis_text(theme: &Theme) -> gpui::Div {
    div()
        .text_size(px(11.0))
        .text_color(theme.text_faint)
        .whitespace_nowrap()
}

fn cartesian(
    chart: &Chart,
    id: &SharedString,
    hidden: &BTreeSet<usize>,
    toggle: Toggle,
    theme: &Theme,
) -> gpui::Div {
    let visible: Vec<usize> = (0..chart.series.len())
        .filter(|s| !hidden.contains(s))
        .collect();
    let values = || {
        visible
            .iter()
            .flat_map(move |s| chart.series[*s].points.iter().map(|p| p.y))
    };
    let mut lo = values().fold(f64::INFINITY, f64::min);
    let mut hi = values().fold(f64::NEG_INFINITY, f64::max);
    if !lo.is_finite() {
        (lo, hi) = (0.0, 1.0);
    }
    if matches!(chart.kind, Kind::Bar | Kind::Area) {
        lo = lo.min(0.0);
        hi = hi.max(0.0);
    }
    let y_ticks = ticks(lo, hi);
    let (y_lo, y_hi) = (
        *y_ticks.first().unwrap_or(&lo),
        *y_ticks.last().unwrap_or(&hi),
    );
    let y_frac = move |v: f64| ((v - y_lo) / (y_hi - y_lo).max(f64::EPSILON)) as f32;
    let unit = chart.unit.clone();

    // Gridlines with their tick labels in the axis column.
    let mut axis = div()
        .relative()
        .w(px(AXIS_WIDTH))
        .h(px(PLOT_HEIGHT))
        .flex_none();
    let mut plot = div().relative().flex_1().min_w_0().h(px(PLOT_HEIGHT));
    for tick in &y_ticks {
        let f = y_frac(*tick);
        axis = axis.child(
            axis_text(theme)
                .absolute()
                .right(px(8.0))
                .top(relative(1.0 - f))
                .mt(px(-7.0))
                .child(format_value(*tick, unit.as_deref())),
        );
        plot = plot.child(
            div()
                .absolute()
                .left_0()
                .right_0()
                .top(relative(1.0 - f))
                .h(px(1.0))
                .bg(if *tick == 0.0 {
                    theme.border_strong
                } else {
                    theme.border
                }),
        );
    }

    let mut x_axis = div()
        .ml(px(AXIS_WIDTH))
        .relative()
        .h(px(16.0))
        .flex()
        .flex_row();
    match chart.kind {
        Kind::Bar => {
            let n = visible.len().max(1) as f32;
            let zero = y_frac(0.0).clamp(0.0, 1.0);
            let mut columns = div().absolute().inset_0().flex().flex_row();
            for (c, label) in chart.labels.iter().enumerate() {
                let mut column = div().relative().flex_1().h_full();
                for (slot, s) in visible.iter().enumerate() {
                    let Some(value) = chart.at(*s, c) else {
                        continue;
                    };
                    let f = y_frac(value).clamp(0.0, 1.0);
                    let (bottom, top) = if f >= zero { (zero, f) } else { (f, zero) };
                    let tip = if chart.series.len() > 1 {
                        format!(
                            "{} · {label}: {}",
                            chart.series[*s].name,
                            format_value(value, unit.as_deref())
                        )
                    } else {
                        format!("{label}: {}", format_value(value, unit.as_deref()))
                    };
                    column = column.child(
                        div()
                            .id(SharedString::from(format!("{id}-bar-{s}-{c}")))
                            .absolute()
                            .left(relative(0.15 + 0.7 * slot as f32 / n))
                            .w(relative(0.7 / n * 0.86))
                            .bottom(relative(bottom))
                            .h(relative((top - bottom).max(0.004)))
                            .rounded_t(px(3.0))
                            .bg(color(theme, *s))
                            .hover(|st| st.opacity(0.8))
                            .tooltip(text_tooltip(tip)),
                    );
                }
                columns = columns.child(column);
            }
            plot = plot.child(columns);
            let every = chart.labels.len().div_ceil(12).max(1);
            for (c, label) in chart.labels.iter().enumerate() {
                x_axis = x_axis.child(
                    axis_text(theme)
                        .flex_1()
                        .min_w_0()
                        .overflow_hidden()
                        .text_center()
                        .when(c % every == 0, |el| el.child(label.clone())),
                );
            }
        }
        Kind::Line | Kind::Area => {
            let count = chart.labels.len();
            let x_frac = move |c: usize| {
                if count <= 1 {
                    0.5
                } else {
                    c as f32 / (count - 1) as f32
                }
            };
            let zero = y_frac(0.0).clamp(0.0, 1.0);
            let area = chart.kind == Kind::Area;
            let lines: Vec<(Hsla, Vec<(f32, f32)>)> = visible
                .iter()
                .map(|s| {
                    let points = (0..count)
                        .filter_map(|c| Some((x_frac(c), y_frac(chart.at(*s, c)?))))
                        .collect();
                    (color(theme, *s), points)
                })
                .collect();
            plot = plot.child(
                canvas(
                    |_, _, _| (),
                    move |bounds, _, window, _| {
                        let at = |(x, y): (f32, f32)| {
                            point(
                                bounds.origin.x + bounds.size.width * x,
                                bounds.origin.y + bounds.size.height * (1.0 - y),
                            )
                        };
                        for (color, points) in &lines {
                            if points.len() < 2 {
                                continue;
                            }
                            if area {
                                let mut fill = PathBuilder::fill();
                                fill.move_to(at((points[0].0, zero)));
                                for p in points {
                                    fill.line_to(at(*p));
                                }
                                fill.line_to(at((points[points.len() - 1].0, zero)));
                                fill.close();
                                if let Ok(path) = fill.build() {
                                    window.paint_path(path, color.opacity(0.18));
                                }
                            }
                            let mut line = PathBuilder::stroke(px(2.0));
                            line.move_to(at(points[0]));
                            for p in &points[1..] {
                                line.line_to(at(*p));
                            }
                            if let Ok(path) = line.build() {
                                window.paint_path(path, *color);
                            }
                        }
                    },
                )
                .absolute()
                .inset_0(),
            );
            for s in &visible {
                for c in 0..count {
                    let Some(value) = chart.at(*s, c) else {
                        continue;
                    };
                    let label = &chart.labels[c];
                    let tip = if chart.series.len() > 1 {
                        format!(
                            "{} · {label}: {}",
                            chart.series[*s].name,
                            format_value(value, unit.as_deref())
                        )
                    } else {
                        format!("{label}: {}", format_value(value, unit.as_deref()))
                    };
                    plot = plot.child(dot(
                        format!("{id}-pt-{s}-{c}").into(),
                        x_frac(c),
                        y_frac(value),
                        color(theme, *s),
                        count <= 40,
                        tip,
                    ));
                }
            }
            let every = count.div_ceil(8).max(1);
            for (c, label) in chart.labels.iter().enumerate() {
                if c % every != 0 && c + 1 != count {
                    continue;
                }
                x_axis = x_axis.child(
                    axis_text(theme)
                        .absolute()
                        .left(relative(x_frac(c)))
                        .ml(px(-30.0))
                        .w(px(60.0))
                        .text_center()
                        .overflow_hidden()
                        .child(label.clone()),
                );
            }
        }
        Kind::Scatter | Kind::Pie => {
            let xs = || {
                visible
                    .iter()
                    .flat_map(move |s| chart.series[*s].points.iter().map(|p| p.x))
            };
            let x_lo = xs().fold(f64::INFINITY, f64::min);
            let x_hi = xs().fold(f64::NEG_INFINITY, f64::max);
            let x_ticks = if x_lo.is_finite() {
                ticks(x_lo, x_hi)
            } else {
                vec![0.0, 1.0]
            };
            let (t_lo, t_hi) = (x_ticks[0], *x_ticks.last().unwrap_or(&1.0));
            let x_frac = move |v: f64| ((v - t_lo) / (t_hi - t_lo).max(f64::EPSILON)) as f32;
            for s in &visible {
                for (p, pt) in chart.series[*s].points.iter().enumerate() {
                    let mut tip = format!(
                        "{}, {}",
                        format_value(pt.x, None),
                        format_value(pt.y, unit.as_deref())
                    );
                    if let Some(label) = &pt.label {
                        tip = format!("{label}: {tip}");
                    } else if chart.series.len() > 1 {
                        tip = format!("{}: {tip}", chart.series[*s].name);
                    }
                    plot = plot.child(dot(
                        format!("{id}-pt-{s}-{p}").into(),
                        x_frac(pt.x),
                        y_frac(pt.y),
                        color(theme, *s),
                        true,
                        tip,
                    ));
                }
            }
            for tick in &x_ticks {
                x_axis = x_axis.child(
                    axis_text(theme)
                        .absolute()
                        .left(relative(x_frac(*tick)))
                        .ml(px(-30.0))
                        .w(px(60.0))
                        .text_center()
                        .child(format_value(*tick, None)),
                );
            }
        }
    }

    let legend = (chart.series.len() > 1).then(|| {
        div().flex().flex_row().flex_wrap().gap(px(4.0)).children(
            chart.series.iter().enumerate().map(|(s, series)| {
                legend_chip(
                    format!("{id}-legend-{s}").into(),
                    series.name.clone(),
                    None,
                    color(theme, s),
                    hidden.contains(&s),
                    s,
                    toggle.clone(),
                    theme,
                )
            }),
        )
    });
    div()
        .w_full()
        .flex()
        .flex_col()
        .gap(px(6.0))
        .children(
            chart
                .y_label
                .clone()
                .map(|label| axis_text(theme).child(label)),
        )
        .child(div().flex().flex_row().child(axis).child(plot))
        .child(x_axis)
        .children(chart.x_label.clone().map(|label| {
            axis_text(theme)
                .ml(px(AXIS_WIDTH))
                .text_center()
                .child(label)
        }))
        .children(legend)
}

/// A point's hover target, centred on its value.
fn dot(
    id: SharedString,
    x: f32,
    y: f32,
    color: Hsla,
    shown: bool,
    tip: String,
) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id)
        .absolute()
        .left(relative(x))
        .top(relative(1.0 - y))
        .ml(px(-6.0))
        .mt(px(-6.0))
        .size(px(12.0))
        .flex()
        .items_center()
        .justify_center()
        .rounded_full()
        .hover(move |s| s.bg(color.opacity(0.25)))
        .tooltip(text_tooltip(tip))
        .child(
            div()
                .size(px(6.0))
                .rounded_full()
                .when(shown, |el| el.bg(color)),
        )
}

fn pie(
    chart: &Chart,
    id: &SharedString,
    hidden: &BTreeSet<usize>,
    toggle: Toggle,
    theme: &Theme,
) -> gpui::Div {
    let points = &chart.series[0].points;
    let total: f64 = points
        .iter()
        .enumerate()
        .filter(|(ix, _)| !hidden.contains(ix))
        .map(|(_, p)| p.y)
        .sum();
    let slices: Vec<(Hsla, f32)> = points
        .iter()
        .enumerate()
        .filter(|(ix, _)| !hidden.contains(ix))
        .map(|(ix, p)| {
            (
                color(theme, ix),
                if total > 0.0 {
                    (p.y / total) as f32
                } else {
                    0.0
                },
            )
        })
        .collect();
    let wheel = canvas(
        |_, _, _| (),
        move |bounds, _, window, _| {
            let center = bounds.center();
            let radius = f32::from(bounds.size.width).min(f32::from(bounds.size.height)) / 2.0;
            let mut start = -std::f32::consts::FRAC_PI_2;
            for (color, share) in &slices {
                if *share <= 0.0 {
                    continue;
                }
                let sweep = std::f32::consts::TAU * share;
                let steps = (sweep / std::f32::consts::TAU * 96.0).ceil().max(2.0) as usize;
                let mut path = PathBuilder::fill();
                path.move_to(center);
                for i in 0..=steps {
                    let angle = start + sweep * i as f32 / steps as f32;
                    path.line_to(point(
                        center.x + px(radius * angle.cos()),
                        center.y + px(radius * angle.sin()),
                    ));
                }
                path.close();
                if let Ok(path) = path.build() {
                    window.paint_path(path, *color);
                }
                start += sweep;
            }
        },
    )
    .size(px(PIE_SIZE))
    .flex_none();
    let unit = chart.unit.clone();
    let legend =
        div()
            .flex()
            .flex_col()
            .gap(px(2.0))
            .min_w_0()
            .children(points.iter().enumerate().map(|(ix, p)| {
                let share = if total > 0.0 && !hidden.contains(&ix) {
                    format!(" ({:.0}%)", p.y / total * 100.0)
                } else {
                    String::new()
                };
                legend_chip(
                    format!("{id}-slice-{ix}").into(),
                    p.label.clone().unwrap_or_default(),
                    Some(format!("{}{share}", format_value(p.y, unit.as_deref()))),
                    color(theme, ix),
                    hidden.contains(&ix),
                    ix,
                    toggle.clone(),
                    theme,
                )
            }));
    div()
        .flex()
        .flex_row()
        .flex_wrap()
        .items_center()
        .gap(px(20.0))
        .child(wheel)
        .child(legend)
}

/// Per-surface chart state: hidden series and source toggles, keyed by the
/// renderer's frame id `"{row_key}-chart-{ix}"`.
#[derive(Default)]
pub(crate) struct ChartState {
    hidden: HashMap<String, BTreeSet<usize>>,
    source: HashSet<String>,
}

impl ChartState {
    fn toggle_series(&mut self, id: &str, ix: usize) {
        let hidden = self.hidden.entry(id.to_owned()).or_default();
        if !hidden.remove(&ix) {
            hidden.insert(ix);
        }
    }
    fn toggle_source(&mut self, id: &str) {
        if !self.source.remove(id) {
            self.source.insert(id.to_owned());
        }
    }
}

/// The transcript row a chart frame belongs to.
pub(crate) fn frame_row(frame_id: &str) -> &str {
    frame_id
        .rsplit_once("-chart-")
        .map_or(frame_id, |(row, _)| row)
}

/// How a chart fence presents itself. `changed` runs after a toggle with the
/// frame id, so the owner can re-measure that row.
pub(crate) fn view(
    code: &str,
    id: SharedString,
    theme: &Theme,
    state: &Rc<RefCell<ChartState>>,
    changed: Rc<dyn Fn(&str, &mut App)>,
) -> DiagramView {
    let chart = match parse(code) {
        Ok(chart) => chart,
        Err(reason) => return DiagramView::Failed(reason.into()),
    };
    let show_source = state.borrow().source.contains(id.as_ref());
    let series_state = state.clone();
    let series_changed = changed.clone();
    let series_id = id.clone();
    let toggle: Toggle = Rc::new(move |ix: usize, _: &mut Window, cx: &mut App| {
        series_state.borrow_mut().toggle_series(&series_id, ix);
        series_changed(&series_id, cx);
    });
    let body = if show_source {
        gpui::Empty.into_any_element()
    } else {
        let hidden = state
            .borrow()
            .hidden
            .get(id.as_ref())
            .cloned()
            .unwrap_or_default();
        render(&chart, id.clone(), &hidden, toggle, theme)
    };
    let source_state = state.clone();
    DiagramView::Diagram(DiagramUi {
        body,
        show_source,
        toggle_source: Rc::new(move |_: &mut Window, cx: &mut App| {
            source_state.borrow_mut().toggle_source(&id);
            changed(&id, cx);
        }),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn open_intelligent_ui_chart_props_parse() {
        let chart = parse(
            r#"{"title":"Revenue","description":"By quarter","data":[{"label":"Q1","value":12},{"label":"Q2","value":"18.5"}]}"#,
        )
        .unwrap();
        assert_eq!(chart.kind, Kind::Bar);
        assert_eq!(chart.title.as_deref(), Some("Revenue"));
        assert_eq!(chart.labels, ["Q1", "Q2"]);
        assert_eq!(chart.series[0].points[1].y, 18.5);
    }

    #[test]
    fn series_share_one_category_order() {
        let chart = parse(
            r#"{"type":"line","series":[
                {"name":"2024","data":[{"label":"Jan","value":1},{"label":"Feb","value":2}]},
                {"name":"2025","data":[{"label":"Feb","value":3},{"label":"Mar","value":4}]}]}"#,
        )
        .unwrap();
        assert_eq!(chart.kind, Kind::Line);
        assert_eq!(chart.labels, ["Jan", "Feb", "Mar"]);
        assert_eq!(chart.at(1, 0), None);
        assert_eq!(chart.at(1, 2), Some(4.0));
    }

    #[test]
    fn scatter_and_pie_have_their_own_rules() {
        let scatter =
            parse(r#"{"type":"scatter","data":[{"x":1,"y":2},{"x":3,"y":5,"label":"B"}]}"#)
                .unwrap();
        assert!(scatter.labels.is_empty());
        assert_eq!(scatter.series[0].points[1].label.as_deref(), Some("B"));
        assert!(parse(r#"{"type":"scatter","data":[{"y":2}]}"#).is_err());
        assert!(parse(r#"{"type":"pie","data":[{"label":"A","value":-1}]}"#).is_err());
        assert!(parse(r#"{"type":"donut","data":[{"label":"A","value":1}]}"#).is_ok());
    }

    #[test]
    fn mistakes_say_what_to_fix() {
        assert!(
            parse("{\"data\": [")
                .unwrap_err()
                .contains("not valid JSON")
        );
        assert!(
            parse(r#"{"type":"radar","data":[]}"#)
                .unwrap_err()
                .contains("radar")
        );
        assert!(
            parse(r#"{"data":[{"label":"A"}]}"#)
                .unwrap_err()
                .contains("numeric `value`")
        );
        assert!(parse(r#"{"title":"Empty"}"#).is_err());
    }

    #[test]
    fn ticks_are_round_and_cover_the_range() {
        assert_eq!(ticks(0.0, 18.5), vec![0.0, 5.0, 10.0, 15.0, 20.0]);
        let t = ticks(-3.0, 7.0);
        assert!(t[0] <= -3.0 && *t.last().unwrap() >= 7.0);
        assert!(ticks(5.0, 5.0).len() >= 2);
    }

    const CODES: [&str; 5] = [
        r#"{"title":"Bar","data":[{"label":"A","value":3},{"label":"B","value":-1}]}"#,
        r#"{"type":"line","series":[{"name":"x","data":[{"label":"1","value":1},{"label":"2","value":4}]},
            {"name":"y","data":[{"label":"1","value":2},{"label":"2","value":3}]}]}"#,
        r#"{"type":"area","unit":"%","data":[{"label":"Mon","value":20},{"label":"Tue","value":35}]}"#,
        r#"{"type":"scatter","data":[{"x":1.5,"y":2},{"x":3,"y":9,"label":"p"}]}"#,
        r#"{"type":"pie","data":[{"label":"Rust","value":70},{"label":"TS","value":30}]}"#,
    ];

    struct Harness {
        state: Rc<RefCell<ChartState>>,
    }

    impl gpui::Render for Harness {
        fn render(&mut self, _: &mut Window, cx: &mut gpui::Context<Self>) -> impl IntoElement {
            let theme = Theme::of(cx).clone();
            let mut column = div().w_full().flex().flex_col();
            for (ix, code) in CODES.iter().enumerate() {
                let changed: Rc<dyn Fn(&str, &mut App)> = Rc::new(|_: &str, _: &mut App| {});
                match view(
                    code,
                    format!("row-chart-{ix}").into(),
                    &theme,
                    &self.state,
                    changed,
                ) {
                    DiagramView::Diagram(diagram) => column = column.child(diagram.body),
                    _ => panic!("chart {ix} did not draw"),
                }
            }
            column
        }
    }

    #[gpui::test]
    fn every_type_draws_and_the_legend_toggles_a_series(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| cx.set_global(Theme::dark()));
        let state: Rc<RefCell<ChartState>> = Rc::default();
        let shared = state.clone();
        let (_, cx) = cx.add_window_view(move |_, _| Harness {
            state: shared.clone(),
        });
        cx.simulate_resize(gpui::size(px(640.0), px(1800.0)));
        cx.update(|window, cx| {
            window.refresh();
            let _ = window.draw(cx);
        });
        let legend = cx
            .debug_bounds("row-chart-1-legend-1")
            .expect("the line chart lists its series");
        cx.simulate_click(legend.center(), gpui::Modifiers::default());
        assert_eq!(
            state.borrow().hidden.get("row-chart-1"),
            Some(&BTreeSet::from([1]))
        );
        assert!(cx.debug_bounds("row-chart-4-slice-0").is_some());
    }

    #[test]
    fn toggles_are_per_frame() {
        let mut state = ChartState::default();
        state.toggle_series("row-1-chart-0", 1);
        state.toggle_series("row-1-chart-0", 2);
        state.toggle_series("row-1-chart-0", 1);
        assert_eq!(state.hidden["row-1-chart-0"], BTreeSet::from([2]));
        assert!(!state.hidden.contains_key("row-1-chart-1"));
        assert_eq!(frame_row("row-1-chart-0"), "row-1");
    }

    #[test]
    fn values_read_naturally() {
        assert_eq!(format_value(1250.0, None), "1,250");
        assert_eq!(format_value(35_000.0, Some("$")), "$35k");
        assert_eq!(format_value(2_400_000.0, None), "2.4M");
        assert_eq!(format_value(0.25, Some("%")), "0.25%");
        assert_eq!(format_value(12.0, Some("ms")), "12 ms");
        assert_eq!(format_value(-40.0, None), "-40");
    }
}
