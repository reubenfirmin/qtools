//! Terminal graphics-protocol capability probe, shared by dq and pq: both render a donut when the
//! terminal supports raster graphics (kitty / iTerm2 / sixel) and fall back to text otherwise.

use crate::term::{save_tty_mode, restore_tty_mode, drain_input, set_noecho};
use std::sync::OnceLock;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Protocol { Kitty, Iterm, Sixel }

static PROTOCOL: OnceLock<Option<Protocol>> = OnceLock::new();

/**
 * Whether the terminal supports any of the raster graphics protocols viuer can drive.
 *
 * Known iTerm-compatible terminals are identified through environment hints. Kitty and Sixel use
 * protocol queries with a device-attributes fallback. viuer reads replies with a blocking read,
 * so we run these probes on a helper thread with a deadline and restore the tty mode afterwards
 * in case a missing reply left it raw. The result is cached for subsequent draws.
 */
pub fn supported() -> bool {
    PROTOCOL.get_or_init(probe).is_some()
}

fn probe() -> Option<Protocol> {
    let debug = std::env::var_os("QTOOLS_DEBUG").is_some();
    let term = std::env::var("TERM").unwrap_or_default();
    if std::env::var_os("TMUX").is_some_and(|value| !value.is_empty())
        || std::env::var_os("STY").is_some_and(|value| !value.is_empty())
        || term.starts_with("tmux") || term.starts_with("screen") {
        if debug { eprintln!("{}[debug]: multiplexer -> text", prog_name()); }
        return None;
    }
    // This check only reads environment variables. Known iTerm-compatible terminals need no
    // blocking query; over SSH without these hints, Kitty/Sixel queries remain available below.
    if viuer::is_iterm_supported() {
        if debug { eprintln!("{}[debug]: iterm=true -> graphics", prog_name()); }
        return Some(Protocol::Iterm);
    }
    let saved = save_tty_mode();
    // Suppress echo for the whole probe window: viuer restores the terminal to cooked/echo between
    // its reads, so a late query reply (e.g. the DA1 hang-guard fallback) would otherwise echo to the
    // screen before we can drain it. We put the original mode back below.
    if let Some(mode) = &saved {
        set_noecho(mode);
    }

    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let kitty = viuer::get_kitty_support();
        let iterm = viuer::is_iterm_supported();
        // Sixel's DA1 probe is only needed (and only worth its query) when kitty/iTerm say no.
        let sixel = if kitty != viuer::KittySupport::None || iterm {
            false
        } else {
            viuer::is_sixel_supported()
        };
        let _ = tx.send((kitty, iterm, sixel));
    });

    let outcome = rx.recv_timeout(std::time::Duration::from_millis(1500));
    if let Some(mode) = &saved {
        // A short settle so a slightly-late query reply is buffered (echo is still off), then discard
        // any unconsumed reply before restoring the original mode, so it can never leak to screen.
        std::thread::sleep(std::time::Duration::from_millis(15));
        drain_input(mode);
        restore_tty_mode(mode);
    }

    interpret_probe(outcome, debug)
}

fn interpret_probe(
    outcome: Result<(viuer::KittySupport, bool, bool), std::sync::mpsc::RecvTimeoutError>,
    debug: bool,
) -> Option<Protocol> {
    match outcome {
        Ok((kitty, iterm, sixel)) => {
            let protocol = if kitty != viuer::KittySupport::None { Some(Protocol::Kitty) }
                else if iterm { Some(Protocol::Iterm) }
                else if sixel { Some(Protocol::Sixel) }
                else { None };
            if debug {
                let kitty = match kitty {
                    viuer::KittySupport::None => "none",
                    viuer::KittySupport::Local => "local",
                    viuer::KittySupport::Remote => "remote"
                };
                eprintln!("{}[debug]: kitty={} iterm={} sixel={} -> {}", prog_name(), kitty, iterm, sixel, protocol.is_some());
            }
            protocol
        }
        Err(_) => {
            if debug {
                eprintln!("{}[debug]: capability probe timed out (>1500ms); terminal never replied", prog_name());
            }
            None
        }
    }
}

/// Size a square chart in terminal cells without exceeding the available columns.
pub fn config(width: Option<usize>, maximum: u32) -> viuer::Config {
    let columns = width.unwrap_or(maximum as usize).min(maximum as usize).max(1) as u32;
    viuer::Config { absolute_offset: false, width: Some(columns), height: Some(columns.div_ceil(2)),
        ..Default::default() }
}

/// Render using only the protocol selected during detection. viuer otherwise probes Sixel first,
/// even after our Kitty/iTerm probe succeeded, which can block outside the detection deadline.
pub fn draw(image: &image::DynamicImage, config: &viuer::Config) -> bool {
    let Some(protocol) = *PROTOCOL.get_or_init(probe) else { return false };
    let config = viuer::Config {
        use_kitty: protocol == Protocol::Kitty,
        use_iterm: protocol == Protocol::Iterm,
        use_sixel: protocol == Protocol::Sixel,
        ..config.clone()
    };
    match viuer::print(image, &config) {
        Ok(_) => true,
        Err(error) => { debug_print_failure(&error); false }
    }
}

/// Log a viuer render failure to stderr when `QTOOLS_DEBUG` is set, so a failed draw isn't silent.
pub fn debug_print_failure(e: &viuer::ViuError) {
    if std::env::var_os("QTOOLS_DEBUG").is_some() {
        eprintln!("{}[debug]: viuer::print failed: {e}", prog_name());
    }
}

/// The running binary's name (e.g. "dq" or "pq"), for debug messages from this shared module.
fn prog_name() -> String {
    std::env::args().next()
        .and_then(|p| std::path::Path::new(&p).file_name().map(|n| n.to_string_lossy().into_owned()))
        .unwrap_or_else(|| "qtools".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn program_name_is_available() {
        assert!(!prog_name().is_empty());
    }

    #[test]
    fn probe_result_accepts_each_protocol_and_rejects_none_or_timeout() {
        use viuer::KittySupport;

        assert_eq!(interpret_probe(Ok((KittySupport::Local, false, false)), false), Some(Protocol::Kitty));
        assert_eq!(interpret_probe(Ok((KittySupport::Remote, false, false)), false), Some(Protocol::Kitty));
        assert_eq!(interpret_probe(Ok((KittySupport::None, true, false)), false), Some(Protocol::Iterm));
        assert_eq!(interpret_probe(Ok((KittySupport::None, false, true)), false), Some(Protocol::Sixel));
        assert_eq!(interpret_probe(Ok((KittySupport::None, false, false)), false), None);
        assert_eq!(interpret_probe(Err(std::sync::mpsc::RecvTimeoutError::Timeout), false), None);
    }
}
