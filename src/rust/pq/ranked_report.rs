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

#[derive(Debug)]
pub struct Report {
    pub mode: &'static str,
    pub title: &'static str,
    pub value_label: &'static str,
    pub value_kind: ValueKind,
    pub rows: Vec<Row>,
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
        self.write_rows(out, options, context, false)
    }

    fn write_rows<W: Write>(&self, out: &mut W, options: &Options, context: RenderContext, swatches: bool) -> io::Result<()> {
        let shown: Vec<&Row> = self.rows.iter().take(options.top).collect();
        if shown.is_empty() {
            writeln!(out, "{}", dim(self.empty_message, context.colors))?;
            return Ok(());
        }
        let largest = shown.iter().map(|row| row.value).max().unwrap_or(0);
        let indent = if swatches { "    " } else { "" };
        writeln!(out, "{}", dim(&format!("{}{:>10}  {:<20}  process", indent, self.value_label, "activity"), context.colors))?;
        for (rank, row) in shown.into_iter().enumerate() {
            if swatches {
                write!(out, "{}  ", color_swatch(style::PALETTE.get(rank).copied().unwrap_or(style::OTHER_COLOR), context.colors))?;
            }
            let fill = if largest > 0 { chart::bar_fill(row.value as f64 / largest as f64, chart::BAR_WIDTH) } else { 0 };
            let bar = format!("{}{}", "#".repeat(fill), ".".repeat(chart::BAR_WIDTH - fill));
            writeln!(out, "{:>10}  {}  {}  {}", value_text(row.value, self.value_kind), dim(&bar, context.colors), row.identity, row.summary)?;
            if options.verbose {
                for detail in &row.details { writeln!(out, "{}  {}", indent, dim(detail, context.colors))?; }
            }
        }
        Ok(())
    }

    fn write_graphics<W: Write>(&self, out: &mut W, options: &Options, context: RenderContext) -> io::Result<()> {
        self.write_graphics_with(out, options, context, |image, config| match viuer::print(image, config) {
            Ok(_) => true,
            Err(error) => { graphics::debug_print_failure(&error); false }
        })
    }

    fn write_graphics_with<W: Write>(&self, out: &mut W, options: &Options, context: RenderContext,
        mut draw: impl FnMut(&image::DynamicImage, &viuer::Config) -> bool) -> io::Result<()> {
        self.write_header(out, context)?;
        if matches!(self.value_kind, ValueKind::Score | ValueKind::Percent) {
            return self.write_rows(out, options, context, false);
        }
        let items: Vec<(String, u64)> = self.rows.iter().map(|row| (row.identity.clone(), row.value)).collect();
        let total = items.iter().map(|(_, value)| *value).sum();
        if total == 0 { return self.write_rows(out, options, context, false); }
        let segments = chart::segments(&items, total, options.top.min(style::PALETTE.len()));
        let image = donut::build_donut_image(&chart::slices(&segments), chart::DONUT_PX);
        out.flush()?;
        let config = viuer::Config { absolute_offset: false, width: Some(40), height: Some(20), ..Default::default() };
        if !draw(&image, &config) {
            return self.write_rows(out, options, context, false);
        }
        self.write_rows(out, options, context, true)
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
            empty_message: "empty" }
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
}
