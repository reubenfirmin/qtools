//! Shared selection of graphics/text/machine render targets and the stdout error boundary used by
//! both pq and dq.

use std::io::{self, BufWriter, IsTerminal, Write};

use crate::output::{OutputFormat, OutputOptions};
use crate::{graphics, term};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RenderTarget { Graphics, Text, Json, JsonLines }

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RenderContext {
    pub target: RenderTarget,
    pub colors: bool,
    pub width: Option<usize>,
}

impl RenderContext {
    pub fn detect(options: OutputOptions) -> Self {
        match options.format {
            OutputFormat::Json => Self { target: RenderTarget::Json, colors: false, width: None },
            OutputFormat::JsonLines => Self { target: RenderTarget::JsonLines, colors: false, width: None },
            OutputFormat::Rich => {
                let interactive = io::stdout().is_terminal() && !options.no_color;
                let target = if interactive && graphics::supported() { RenderTarget::Graphics } else { RenderTarget::Text };
                Self { target, colors: interactive, width: interactive.then(term::stdout_width).flatten() }
            }
        }
    }

    pub fn machine(self) -> bool {
        matches!(self.target, RenderTarget::Json | RenderTarget::JsonLines)
    }

    pub fn compact_json(self) -> bool { self.target == RenderTarget::JsonLines }
}

/// Own a buffered stdout, flush it, and give every qtool the same quiet broken-pipe behavior.
pub fn stdout(tool: &str, render: impl FnOnce(&mut BufWriter<io::Stdout>) -> io::Result<()>) {
    let mut out = BufWriter::new(io::stdout());
    let result = render(&mut out).and_then(|_| out.flush());
    match result {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::BrokenPipe => std::process::exit(0),
        Err(e) => { eprintln!("{tool}: error writing output: {e}"); std::process::exit(1); }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn machine_formats_have_deterministic_contexts() {
        let base = OutputOptions { format: OutputFormat::Json, top: None, verbose: false, no_color: false };
        assert_eq!(RenderContext::detect(base).target, RenderTarget::Json);
        assert_eq!(RenderContext::detect(OutputOptions { format: OutputFormat::JsonLines, ..base }).target, RenderTarget::JsonLines);
        assert!(!RenderContext::detect(base).colors);
    }
}
