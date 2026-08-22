//! Output choices shared by qtools query plans.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputFormat {
    Rich,
    Json,
    JsonLines,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OutputOptions {
    pub format: OutputFormat,
    pub top: Option<usize>,
    pub verbose: bool,
    pub no_color: bool,
}

impl OutputOptions {
    pub fn machine(self) -> bool {
        matches!(self.format, OutputFormat::Json | OutputFormat::JsonLines)
    }

    pub fn compact_json(self) -> bool {
        self.format == OutputFormat::JsonLines
    }
}
