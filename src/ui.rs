//! How gritty talks to a human.
//!
//! Every line gritty prints that is *not* a session's own output goes through
//! here. The point is that a message's severity is a named thing rather than an
//! escape sequence picked at the call site: five levels, one place that decides
//! what each looks like, and an ASCII fallback for terminals whose locale
//! cannot render the marker glyph.
//!
//! **Styling is emitted unconditionally by [`format`]; whether those bytes
//! survive is the sink's decision.** The CLI sinks below print through
//! `anstream`, which strips ANSI when the destination is not a terminal and
//! honors `NO_COLOR`, `CLICOLOR`, `CLICOLOR_FORCE`, `TERM=dumb`, and `--color`.
//! The client writes into a terminal in raw mode via a bare fd, so it renders
//! with [`terminal_line`] / [`terminal_body`], which apply the same decision by
//! hand.
//!
//! That split is load-bearing. gritty's stdout also carries opaque payload
//! bytes -- `receive -` and the PTY relay -- and those must never be filtered.
//! Because they write to raw fds and never reach a sink here, no code path can
//! strip escape sequences out of transferred data.
//!
//! Two surfaces deliberately stay outside this module:
//!
//! - The **session picker and prune TUI** (`commands/session.rs`) paint a raw
//!   terminal they only ever enter when stderr is a tty, with their own colors.
//! - **Notices the server injects into a client's terminal** (`server.rs`: tail
//!   fell behind, output lost while disconnected) are styled unconditionally.
//!   The daemon may be on another machine; it cannot see the client's
//!   `NO_COLOR`, `--color`, or locale, so neither the strip decision nor the
//!   marker glyph is knowable there. Teaching it would take a protocol
//!   capability, not a refactor.

use std::sync::OnceLock;

pub use anstream::ColorChoice;

/// What a message *is*. Determines the color, and whether the line is narration
/// (marker-prefixed) or a severity report (word-prefixed).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Level {
    /// A thing gritty finished doing: `server started (pid 123)`.
    Success,
    /// A thing gritty is doing, or a benign notice: `starting server...`.
    Status,
    /// Something is wrong but the command continues.
    Warn,
    /// The command (or the session) failed.
    Error,
    /// Secondary detail, subordinate to a nearby line.
    Detail,
}

/// How the rendered line terminates.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum LineEnding {
    /// No terminator -- the caller is composing a larger line.
    None,
    /// `\n`: ordinary CLI output.
    Lf,
    /// `\r\n`: a terminal in raw mode does not return to column 0 on its own.
    CrLf,
}

/// The whole palette. Nothing outside this module spells an SGR sequence: a
/// caller that needs to paint something which is not a [`Level`] message -- a
/// progress bar, a table row, a parenthetical annotation -- composes from here.
pub mod sgr {
    pub const GREEN: &str = "\x1b[32m";
    pub const DIM_YELLOW: &str = "\x1b[2;33m";
    pub const YELLOW: &str = "\x1b[33m";
    pub const RED: &str = "\x1b[31m";
    pub const DIM: &str = "\x1b[2m";
    pub const BOLD: &str = "\x1b[1m";
    pub const RESET: &str = "\x1b[0m";
}

use sgr::{DIM, DIM_YELLOW, GREEN, RED, RESET, YELLOW};

/// Marks a line as gritty narrating, rather than output from the program
/// running inside the session. Errors and warnings say `error:` / `warning:`
/// instead: that word is the marker, and it is what a human greps for.
const MARKER: &str = "\u{25b8}"; // ▸
const MARKER_ASCII: &str = ">";

/// Pick the marker glyph for a `LC_ALL`/`LC_CTYPE`/`LANG` value, in POSIX
/// precedence order. `None` means none of them was set to a non-empty value,
/// which is the `C`/`POSIX` locale -- ASCII only.
fn marker_for(locale: Option<&str>) -> &'static str {
    let utf8 = locale.is_some_and(|v| {
        let v = v.to_ascii_lowercase();
        v.contains("utf-8") || v.contains("utf8")
    });
    if utf8 { MARKER } else { MARKER_ASCII }
}

/// The marker glyph for this process's locale. Resolved once: the locale cannot
/// change under a running process in any way that matters here.
pub fn marker() -> &'static str {
    static CACHED: OnceLock<&'static str> = OnceLock::new();
    CACHED.get_or_init(|| {
        let locale = ["LC_ALL", "LC_CTYPE", "LANG"]
            .iter()
            .find_map(|k| std::env::var(k).ok().filter(|v| !v.is_empty()));
        marker_for(locale.as_deref())
    })
}

/// Render one message. Always styled -- see the module docs on why the decision
/// to keep or strip the escapes belongs to the sink, not here.
///
/// Pure, so the whole vocabulary is testable without a terminal.
fn render(level: Level, text: &str, marker: &str, ending: LineEnding) -> String {
    let (sgr, body) = match level {
        Level::Success => (GREEN, format!("{marker} {text}")),
        Level::Status => (DIM_YELLOW, format!("{marker} {text}")),
        Level::Detail => (DIM, format!("{marker} {text}")),
        Level::Warn => (YELLOW, format!("warning: {text}")),
        Level::Error => (RED, format!("error: {text}")),
    };
    let terminator = match ending {
        LineEnding::None => "",
        LineEnding::Lf => "\n",
        LineEnding::CrLf => "\r\n",
    };
    format!("{sgr}{body}{RESET}{terminator}")
}

/// Render one message using this process's marker glyph.
pub fn format(level: Level, text: &str, ending: LineEnding) -> String {
    render(level, text, marker(), ending)
}

/// Wrap `text` in an [`sgr`] sequence and a reset. For the things that are not
/// [`Level`] messages -- status glyphs, table rows, parenthetical annotations.
/// Like [`format`], it always styles; the sink decides whether that survives.
pub fn paint(sgr: &str, text: &str) -> String {
    format!("{sgr}{text}{RESET}")
}

/// Override the color decision. Called once from `main`, before any output.
/// `Auto` is already the default, so passing it changes nothing and leaves
/// `NO_COLOR` and friends in charge.
pub fn set_color_choice(choice: ColorChoice) {
    choice.write_global();
}

/// Whether ANSI written to stdout right now would be honored. Only the client's
/// raw-fd path needs this; every other caller prints through an `anstream` sink
/// that decides for itself.
///
/// Deliberately not memoized: `main` writes the global color choice after the
/// first `Cli::parse()`, and a cache filled before that would pin the wrong
/// answer for the life of the process.
pub fn stdout_is_colored() -> bool {
    anstream::AutoStream::choice(&std::io::stdout()) != ColorChoice::Never
}

/// As [`stdout_is_colored`], for stderr. Used by the transfer progress bar,
/// which cannot print through an `anstream` sink: the sink would strip its
/// `\x1b[2K` erase-line along with the color, and the bar repaints in place.
pub fn stderr_is_colored() -> bool {
    anstream::AutoStream::choice(&std::io::stderr()) != ColorChoice::Never
}

/// Whether stderr is an interactive terminal. Gates *motion* -- progress bars,
/// in-place repaints -- which is a different question from whether color is
/// wanted: `--color=never` on a tty still wants a progress bar, and a progress
/// bar redirected to a file is line noise however it is painted.
pub fn stderr_is_interactive() -> bool {
    std::io::IsTerminal::is_terminal(&std::io::stderr())
}

/// Render for the client's raw-mode terminal, stripping ANSI when color is off.
/// The client writes to the stdout fd directly and so cannot lean on a sink.
pub fn terminal_body(level: Level, text: &str) -> String {
    strip_unless_colored(format(level, text, LineEnding::None))
}

/// [`terminal_body`] plus a `\r\n` terminator.
pub fn terminal_line(level: Level, text: &str) -> String {
    strip_unless_colored(format(level, text, LineEnding::CrLf))
}

fn strip_unless_colored(s: String) -> String {
    if stdout_is_colored() { s } else { anstream::adapter::strip_str(&s).to_string() }
}

// -- CLI sinks. Diagnostics go to stderr; stdout is reserved for data. --------

/// A thing gritty finished doing.
pub fn success(text: &str) {
    emit(Level::Success, text);
}

/// A thing gritty is doing, or a benign notice.
pub fn status(text: &str) {
    emit(Level::Status, text);
}

/// Something is wrong; the command continues.
pub fn warn(text: &str) {
    emit(Level::Warn, text);
}

/// The command failed. Rendered `error: <text>`.
pub fn error(text: &str) {
    emit(Level::Error, text);
}

/// Secondary detail, subordinate to a nearby line.
pub fn detail(text: &str) {
    emit(Level::Detail, text);
}

fn emit(level: Level, text: &str) {
    anstream::eprintln!("{}", format(level, text, LineEnding::None));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn narration_levels_carry_the_marker() {
        for (level, sgr) in
            [(Level::Success, GREEN), (Level::Status, DIM_YELLOW), (Level::Detail, DIM)]
        {
            assert_eq!(render(level, "hi", "@", LineEnding::None), format!("{sgr}@ hi{RESET}"));
        }
    }

    /// `error:` / `warning:` *are* the marker -- and they are what a human and a
    /// grep both look for. Adding the glyph too would be noise.
    #[test]
    fn severity_levels_say_the_word_instead_of_the_glyph() {
        assert_eq!(
            render(Level::Error, "boom", "@", LineEnding::None),
            format!("{RED}error: boom{RESET}")
        );
        assert_eq!(
            render(Level::Warn, "hmm", "@", LineEnding::None),
            format!("{YELLOW}warning: hmm{RESET}")
        );
    }

    /// The terminator lands after the reset, so a truncated write never leaves
    /// the terminal in a colored state.
    #[test]
    fn line_endings_are_appended_outside_the_reset() {
        let ending = |e| render(Level::Success, "x", "@", e);
        assert_eq!(ending(LineEnding::None), format!("{GREEN}@ x{RESET}"));
        assert_eq!(ending(LineEnding::Lf), format!("{GREEN}@ x{RESET}\n"));
        assert_eq!(ending(LineEnding::CrLf), format!("{GREEN}@ x{RESET}\r\n"));
    }

    #[test]
    fn utf8_locales_get_the_glyph_and_others_get_ascii() {
        for utf8 in ["en_US.UTF-8", "C.utf8", "en_GB.utf-8"] {
            assert_eq!(marker_for(Some(utf8)), MARKER, "{utf8}");
        }
        for ascii in ["C", "POSIX", "en_US", "en_US.ISO-8859-1"] {
            assert_eq!(marker_for(Some(ascii)), MARKER_ASCII, "{ascii}");
        }
    }

    /// No locale set at all is the `C` locale -- a bare container, a cron job.
    /// Emitting a multi-byte glyph there produces mojibake.
    #[test]
    fn absent_locale_falls_back_to_ascii() {
        assert_eq!(marker_for(None), MARKER_ASCII);
    }

    /// Whatever a sink later decides, the rendered bytes must be strippable back
    /// to exactly the text a non-terminal should see.
    #[test]
    fn stripping_a_rendered_line_leaves_plain_text() {
        let styled = render(Level::Error, "no server running", "@", LineEnding::Lf);
        let plain = anstream::adapter::strip_str(&styled).to_string();
        assert_eq!(plain, "error: no server running\n");
    }
}
