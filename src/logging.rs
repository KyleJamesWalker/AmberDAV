//! Logging setup (issue #47): timestamps + levels via `tracing`, trimmed for
//! the static device builds — no env-filter regex machinery (verbosity is a
//! single level word) and no ANSI color (device logs land in `log.txt`).
//!
//! Verbosity follows the repo's precedence philosophy (CLI > env > default):
//! `--verbose` forces debug; otherwise `AMBERDAV_LOG`, then `RUST_LOG`, each
//! read as one of `off|error|warn|info|debug|trace`; the default is info.
//!
//! The startup banner + QR on stdout is user-facing output, not logging —
//! it stays plain `println!`, and the generated password appears there
//! (deliberately) and in no log line.

use tracing::level_filters::LevelFilter;

/// Install the global subscriber. Called once, first thing in `main`, so the
/// config-load diagnostics already go through it.
pub fn init(verbose: bool) {
    let level = level_from(
        verbose,
        std::env::var("AMBERDAV_LOG").ok().as_deref(),
        std::env::var("RUST_LOG").ok().as_deref(),
    );
    tracing_subscriber::fmt()
        .with_max_level(level)
        // stderr, like the eprintln! lines this replaces — the on-device
        // launcher already captures both streams into log.txt.
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .init();
}

/// The effective level: `--verbose` wins, then the env vars in order, then
/// info. Pure so the precedence is host-testable.
fn level_from(verbose: bool, amberdav_log: Option<&str>, rust_log: Option<&str>) -> LevelFilter {
    if verbose {
        return LevelFilter::DEBUG;
    }
    amberdav_log
        .and_then(parse_level)
        .or_else(|| rust_log.and_then(parse_level))
        .unwrap_or(LevelFilter::INFO)
}

/// One level word, case-insensitive. Unknown words (including `RUST_LOG`
/// module-filter syntax like `hyper=debug`, which this deliberately does not
/// implement) fall through to the caller's default.
fn parse_level(word: &str) -> Option<LevelFilter> {
    match word.trim().to_ascii_lowercase().as_str() {
        "off" | "none" => Some(LevelFilter::OFF),
        "error" => Some(LevelFilter::ERROR),
        "warn" | "warning" => Some(LevelFilter::WARN),
        "info" => Some(LevelFilter::INFO),
        "debug" => Some(LevelFilter::DEBUG),
        "trace" => Some(LevelFilter::TRACE),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn verbose_wins_over_everything() {
        assert_eq!(
            level_from(true, Some("error"), Some("trace")),
            LevelFilter::DEBUG
        );
    }

    #[test]
    fn env_precedence_is_amberdav_then_rust_log_then_info() {
        assert_eq!(
            level_from(false, Some("trace"), Some("error")),
            LevelFilter::TRACE
        );
        assert_eq!(level_from(false, None, Some("warn")), LevelFilter::WARN);
        assert_eq!(level_from(false, None, None), LevelFilter::INFO);
        // An unparseable AMBERDAV_LOG falls through to RUST_LOG, then info.
        assert_eq!(
            level_from(false, Some("loud"), Some("error")),
            LevelFilter::ERROR
        );
        assert_eq!(
            level_from(false, Some("hyper=debug"), None),
            LevelFilter::INFO
        );
    }

    #[test]
    fn level_words_parse_case_insensitively() {
        assert_eq!(parse_level(" Debug "), Some(LevelFilter::DEBUG));
        assert_eq!(parse_level("WARN"), Some(LevelFilter::WARN));
        assert_eq!(parse_level("warning"), Some(LevelFilter::WARN));
        assert_eq!(parse_level("off"), Some(LevelFilter::OFF));
        assert_eq!(parse_level(""), None);
        assert_eq!(parse_level("verbose"), None);
    }
}
