use crate::config::ExplorerIconMode;
use crate::workspace::DiagnosticSeverity;

#[must_use]
pub fn severity_glyph(severity: DiagnosticSeverity, mode: ExplorerIconMode) -> &'static str {
    use DiagnosticSeverity::{Error, Hint, Information, Warning};
    if mode == ExplorerIconMode::NerdFont {
        match severity {
            Error => "\u{f057}",
            Warning => "\u{f071}",
            Information => "\u{f05a}",
            Hint => "\u{f0eb}",
        }
    } else {
        match severity {
            Error => "E",
            Warning => "W",
            Information => "I",
            Hint => "H",
        }
    }
}
