//! Shared presentation for pq diagnostic views that rank identities by one directly attributable
//! quantity. Domain collectors remain typed; this is only their common human/machine report DTO.

use std::io::{self, Write};

use crate::{chart, donut, graphics, style};
use crate::render::{RenderContext, RenderTarget};
use crate::style::{bold, color_swatch, dim, human_plain};
use serde::Serialize;

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ValueKind { Bytes, Count, Score, Percent }

#[derive(Debug, Serialize)]
pub struct Row {
    pub identity: String,
    pub value: u64,
    pub summary: String,
    pub details: Vec<String>,
}

/// One independently-normalized ring in a multi-ring graphic. `values` is aligned with `rows`,
/// which keeps an identity's palette color stable across rings even when its value is zero in one.
/// Text and JSON retain their existing row-oriented representation.
#[derive(Debug)]
pub struct Ring {
    pub label: &'static str,
    pub values: Vec<u64>,
}

/// Optional aligned columns for reports whose dimensions deserve first-class table cells rather
/// than a prose summary. Rows are aligned with `Report::rows`; all cells are right-aligned and the
/// process identity remains the final column.
#[derive(Debug)]
pub struct Table {
    pub headers: Vec<&'static str>,
    pub rows: Vec<Vec<String>>,
}

#[derive(Debug)]
pub struct Report {
    pub mode: &'static str,
    pub title: &'static str,
    pub value_label: &'static str,
    pub value_kind: ValueKind,
    pub rows: Vec<Row>,
    pub rings: Vec<Ring>,
    pub table: Option<Table>,
    pub empty_message: &'static str,
}

pub struct Options { pub top: usize, pub verbose: bool, pub sample_interval_ms: Option<u64> }

impl Report {
    pub fn render<W: Write>(&self, out: &mut W, options: &Options, context: RenderContext) -> io::Result<()> {
        match context.target {
            RenderTarget::Json | RenderTarget::JsonLines => self.write_json(out, options, context.compact_json()),
            RenderTarget::Graphics => self.write_graphics(out, options, context),
            RenderTarget::Text => self.write_text(out, options, context),
        }
    }

    fn write_header<W: Write>(&self, out: &mut W, context: RenderContext) -> io::Result<()> {
        writeln!(out, "{}  {}  {}", bold("pq", context.colors), dim("|", context.colors), self.title)?;
        writeln!(out, "{}", dim("----------------------------------------------------", context.colors))
    }

    fn write_text<W: Write>(&self, out: &mut W, options: &Options, context: RenderContext) -> io::Result<()> {
        self.write_header(out, context)?;
        self.write_rows(out, options, context, false, true)
    }

    fn write_rows<W: Write>(&self, out: &mut W, options: &Options, context: RenderContext,
        swatches: bool, summaries: bool) -> io::Result<()> {
        let shown: Vec<&Row> = self.rows.iter().take(options.top).collect();
        if shown.is_empty() {
            writeln!(out, "{}", dim(self.empty_message, context.colors))?;
            return Ok(());
        }
        if let Some(table) = &self.table {
            return self.write_table(out, &shown, table, options, context, swatches);
        }
        let largest = shown.iter().map(|row| row.value).max().unwrap_or(0);
        let row_colors = chart::identity_colors(shown.iter().map(|row| row.identity.as_str()));
        let indent = if swatches { "    " } else { "" };
        writeln!(out, "{}", dim(&format!("{}{:>10}  {:<20}  process", indent, self.value_label, "activity"), context.colors))?;
        for (rank, row) in shown.into_iter().enumerate() {
            if swatches {
                write!(out, "{}  ", color_swatch(row_colors[rank], context.colors))?;
            }
            let fill = if largest > 0 { chart::bar_fill(row.value as f64 / largest as f64, chart::BAR_WIDTH) } else { 0 };
            let bar = format!("{}{}", "#".repeat(fill), ".".repeat(chart::BAR_WIDTH - fill));
            let summary = if summaries && !row.summary.is_empty() { format!("  {}", row.summary) } else { String::new() };
            writeln!(out, "{:>10}  {}  {}{}", value_text(row.value, self.value_kind), dim(&bar, context.colors), row.identity, summary)?;
            if options.verbose {
                for detail in &row.details { writeln!(out, "{}  {}", indent, dim(detail, context.colors))?; }
            }
        }
        Ok(())
    }

    fn write_table<W: Write>(&self, out: &mut W, shown: &[&Row], table: &Table, options: &Options,
        context: RenderContext, swatches: bool) -> io::Result<()> {
        let widths: Vec<usize> = table.headers.iter().enumerate().map(|(column, header)| {
            table.rows.iter().take(shown.len()).filter_map(|row| row.get(column))
                .map(|cell| cell.len()).fold(header.len(), usize::max)
        }).collect();
        let numeric: Vec<bool> = table.headers.iter().enumerate().map(|(column, _)| {
            table.rows.iter().take(shown.len()).filter_map(|row| row.get(column))
                .all(|cell| numeric_cell(cell))
        }).collect();
        let indent = if swatches { "    " } else { "" };
        let mut heading = indent.to_string();
        for (column, header) in table.headers.iter().enumerate() {
            if numeric[column] {
                heading.push_str(&format!("{:>width$}  ", header, width = widths[column]));
            } else {
                heading.push_str(&format!("{:<width$}  ", header, width = widths[column]));
            }
        }
        heading.push_str("process");
        writeln!(out, "{}", dim(&heading, context.colors))?;
        let row_colors = chart::identity_colors(shown.iter().map(|row| row.identity.as_str()));
        for (rank, row) in shown.iter().enumerate() {
            if swatches { write!(out, "{}  ", color_swatch(row_colors[rank], context.colors))?; }
            let cells = table.rows.get(rank);
            for (column, width) in widths.iter().enumerate() {
                let cell = cells.and_then(|row| row.get(column)).map(String::as_str).unwrap_or("");
                if numeric[column] {
                    write!(out, "{:>width$}  ", cell, width = width)?;
                } else {
                    write!(out, "{:<width$}  ", cell, width = width)?;
                }
            }
            writeln!(out, "{}", row.identity)?;
            if options.verbose {
                for detail in &row.details { writeln!(out, "{}  {}", indent, dim(detail, context.colors))?; }
            }
        }
        Ok(())
    }

    fn write_graphics<W: Write>(&self, out: &mut W, options: &Options, context: RenderContext) -> io::Result<()> {
        self.write_graphics_with(out, options, context, graphics::draw)
    }

    fn write_graphics_with<W: Write>(&self, out: &mut W, options: &Options, context: RenderContext,
        mut draw: impl FnMut(&image::DynamicImage, &viuer::Config) -> bool) -> io::Result<()> {
        self.write_header(out, context)?;
        if matches!(self.value_kind, ValueKind::Score | ValueKind::Percent) {
            return self.write_rows(out, options, context, false, true);
        }
        if !self.rings.is_empty() {
            let rings = self.ring_slices(options);
            if rings.is_empty() { return self.write_rows(out, options, context, false, true); }
            let image = donut::build_core_rings_image(
                &rings.iter().map(|(_, slices)| slices.clone()).collect::<Vec<_>>(), chart::DONUT_PX);
            out.flush()?;
            let config = graphics::config(context.width, 40);
            if !draw(&image, &config) {
                return self.write_rows(out, options, context, false, true);
            }
            let labels = rings.iter().map(|(label, _)| *label).collect::<Vec<_>>().join(" · ");
            writeln!(out, "{}", dim(&format!("rings outside → in: {labels}"), context.colors))?;
            return self.write_rows(out, options, context, true, false);
        }
        let items: Vec<(String, u64)> = self.rows.iter().map(|row| (row.identity.clone(), row.value)).collect();
        let total = items.iter().map(|(_, value)| *value).sum();
        if total == 0 { return self.write_rows(out, options, context, false, true); }
        let segments = chart::identity_segments(&items, total, options.top);
        let image = donut::build_donut_image(&chart::slices(&segments), chart::DONUT_PX);
        out.flush()?;
        let config = graphics::config(context.width, 40);
        if !draw(&image, &config) {
            return self.write_rows(out, options, context, false, true);
        }
        self.write_rows(out, options, context, true, true)
    }

    /// Build category rings using each row's stable identity color, rather than category-local
    /// rank. An identity therefore keeps the same color in every ring. Empty categories are omitted.
    fn ring_slices<'a>(&'a self, options: &Options) -> Vec<(&'a str, Vec<donut::Slice>)> {
        let visible = self.rows.len().min(options.top).min(style::PALETTE.len());
        let colors = chart::identity_colors(self.rows.iter().take(visible).map(|row| row.identity.as_str()));
        self.rings.iter().filter_map(|ring| {
            let total: u64 = ring.values.iter().sum();
            if total == 0 { return None; }
            let mut slices: Vec<donut::Slice> = (0..visible).map(|rank| {
                (ring.values.get(rank).copied().unwrap_or(0), colors[rank])
            }).collect();
            let shown: u64 = slices.iter().map(|(value, _)| *value).sum();
            if total > shown { slices.push((total - shown, style::OTHER_COLOR)); }
            Some((ring.label, slices))
        }).collect()
    }

    fn write_json<W: Write>(&self, out: &mut W, options: &Options, compact: bool) -> io::Result<()> {
        #[derive(Serialize)]
        struct Json<'a> { mode: &'static str, generated_at_unix_ms: u128, sample_interval_ms: Option<u64>, value_kind: ValueKind, rows: &'a [Row] }
        let generated_at_unix_ms = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_millis();
        let end = self.rows.len().min(options.top);
        let report = Json { mode: self.mode, generated_at_unix_ms, sample_interval_ms: options.sample_interval_ms,
            value_kind: self.value_kind, rows: &self.rows[..end] };
        if compact { serde_json::to_writer(&mut *out, &report) } else { serde_json::to_writer_pretty(&mut *out, &report) }
            .map_err(io::Error::other)?;
        writeln!(out)
    }
}

fn numeric_cell(value: &str) -> bool {
    value == "unlimited" || value.as_bytes().first().is_some_and(|byte| byte.is_ascii_digit() || *byte == b'-')
}

fn value_text(value: u64, kind: ValueKind) -> String {
    match kind {
        ValueKind::Bytes => human_plain(value),
        ValueKind::Count | ValueKind::Score => value.to_string(),
        ValueKind::Percent => format!("{value}%"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn report() -> Report {
        Report { mode: "test", title: "test", value_label: "items", value_kind: ValueKind::Count,
            rows: vec![Row { identity: "alpha".into(), value: 10, summary: "ten".into(), details: vec!["pid 1".into()] }],
            rings: Vec::new(), table: None, empty_message: "empty" }
    }

    #[test]
    fn text_has_bars_and_json_has_mode() {
        let options = Options { top: 15, verbose: true, sample_interval_ms: None };
        let context = RenderContext { target: RenderTarget::Text, colors: false, width: None };
        let mut out = Vec::new();
        report().render(&mut out, &options, context).unwrap();
        assert!(String::from_utf8(out).unwrap().contains("####################"));

        let context = RenderContext { target: RenderTarget::Json, colors: false, width: None };
        let mut out = Vec::new();
        report().render(&mut out, &options, context).unwrap();
        let value: serde_json::Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(value["mode"], "test");
    }


    #[test]
    fn empty_and_non_chartable_graphics_use_text_rows() {
        let options = Options { top: 15, verbose: false, sample_interval_ms: Some(0) };
        let graphics = RenderContext { target: RenderTarget::Graphics, colors: false, width: Some(40) };
        for kind in [ValueKind::Score, ValueKind::Percent] {
            let mut report = report();
            report.value_kind = kind;
            let mut output = Vec::new();
            report.render(&mut output, &options, graphics).unwrap();
            assert!(String::from_utf8(output).unwrap().contains("alpha"));
        }
        let empty = Report { rows: Vec::new(), ..report() };
        let mut output = Vec::new();
        empty.render(&mut output, &options, RenderContext { target: RenderTarget::Text, colors: false, width: None }).unwrap();
        assert!(String::from_utf8(output).unwrap().contains("empty"));
        assert_eq!(value_text(7, ValueKind::Bytes), "7");
        assert_eq!(value_text(7, ValueKind::Score), "7");
        assert_eq!(value_text(7, ValueKind::Percent), "7%");

        for succeeds in [true, false] {
            let mut output = Vec::new();
            report().write_graphics_with(&mut output, &options, graphics, |image, config| {
                assert!(image.width() > 0);
                assert_eq!(config.width, Some(40));
                succeeds
            }).unwrap();
            assert!(String::from_utf8(output).unwrap().contains("alpha"));
        }
    }

    #[test]
    fn category_rings_keep_row_colors_fold_hidden_values_and_replace_graphical_summaries() {
        let mut report = report();
        report.rows.push(Row { identity: "beta".into(), value: 7, summary: "seven".into(), details: Vec::new() });
        report.rows.push(Row { identity: "gamma".into(), value: 5, summary: "five".into(), details: Vec::new() });
        report.rings = vec![
            Ring { label: "files", values: vec![0, 3, 7] },
            Ring { label: "sockets", values: vec![0, 0, 0] },
        ];
        report.table = Some(Table { headers: vec!["files", "anon"], rows: vec![
            vec!["0".into(), "10".into()], vec!["3".into(), "4".into()], vec!["7".into(), "1".into()]
        ] });
        let options = Options { top: 2, verbose: false, sample_interval_ms: None };
        let rings = report.ring_slices(&options);
        assert_eq!(rings.len(), 1);
        assert_eq!(rings[0].0, "files");
        let colors = chart::identity_colors(["alpha", "beta"]);
        assert_eq!(rings[0].1, vec![(0, colors[0]), (3, colors[1]), (7, style::OTHER_COLOR)]);

        let graphics = RenderContext { target: RenderTarget::Graphics, colors: false, width: Some(40) };
        let mut output = Vec::new();
        report.write_graphics_with(&mut output, &options, graphics, |_image, _config| true).unwrap();
        let text = String::from_utf8(output).unwrap();
        assert!(text.contains("rings outside → in: files"));
        assert!(text.contains("files  anon  process"));
        assert!(!text.contains("ten"));

        let mut fallback = Vec::new();
        report.write_graphics_with(&mut fallback, &options, graphics, |_image, _config| false).unwrap();
        let fallback = String::from_utf8(fallback).unwrap();
        assert!(fallback.contains("files  anon  process"));
        assert!(!fallback.contains("rings outside"));
    }
}
