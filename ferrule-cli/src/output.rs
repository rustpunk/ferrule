#![allow(dead_code, unused_variables, unused_imports)]

use ferrule_core::formatter::OutputFormat;
use is_terminal::IsTerminal;

/// Determine the default output format based on TTY detection.
pub fn default_format() -> OutputFormat {
    if std::io::stdout().is_terminal() {
        OutputFormat::Table
    } else {
        OutputFormat::Json
    }
}
