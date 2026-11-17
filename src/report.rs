//! The dry-run table printed on stdout (German). Pure formatting, no I/O.

use std::fmt::Write as _;

use crate::notify::message::warning_text;
use crate::runner::{ReportedTip, TenantError, TenantOutcome, TenantReport};

/// Renders the tables of all tenants (in the given order), separated by one
/// blank line. Empty for no tenants.
pub fn render_all(outcomes: &[TenantOutcome]) -> String {
    outcomes
        .iter()
        .map(|o| dry_run_table(&o.name, &o.result))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Renders one tenant's dry-run result as a table: one row per tip
/// (teams, tip, expected points and, for already tipped matches, the placed
/// tip as `bisher X:Y`), followed by the warnings. Ends with `\n`.
pub fn dry_run_table(tenant: &str, result: &Result<TenantReport, TenantError>) -> String {
    let report = match result {
        Ok(r) => r,
        Err(e) => return format!("{tenant} – Fehler (dry run)\n  {e}\n"),
    };

    let mut out = match report.matchdays.as_slice() {
        [] => format!("{tenant} (dry run)\n"),
        [md] => format!("{tenant} – Spieltag {md} (dry run)\n"),
        mds => {
            let mds: Vec<String> = mds.iter().map(u32::to_string).collect();
            format!("{tenant} – Spieltage {} (dry run)\n", mds.join(", "))
        }
    };

    if report.tips.is_empty() {
        if report.warnings.is_empty() {
            out.push_str("  nichts zu tippen\n");
        } else {
            out.push_str("  keine Tipps berechnet\n");
        }
    } else {
        write_rows(&mut out, &report.tips, report.matchdays.len() > 1);
    }

    for w in &report.warnings {
        let _ = writeln!(out, "  Warnung: {}", warning_text(w));
    }
    out
}

/// One aligned row per tip, sorted by matchday (stable). Grouped under a
/// "Spieltag N:" sub-heading if the window or the tips span several matchdays.
fn write_rows(out: &mut String, tips: &[ReportedTip], several_matchdays: bool) {
    let mut sorted: Vec<&ReportedTip> = tips.iter().collect();
    sorted.sort_by_key(|t| t.matchday);
    let grouped = several_matchdays
        || sorted.first().map(|t| t.matchday) != sorted.last().map(|t| t.matchday);

    // Widths in chars, not bytes: team names contain umlauts.
    let home_width = sorted.iter().map(|t| t.home.chars().count()).max();
    let away_width = sorted.iter().map(|t| t.away.chars().count()).max();
    let tip_texts: Vec<String> = sorted.iter().map(|t| t.tip.to_string()).collect();
    let tip_width = tip_texts.iter().map(|t| t.chars().count()).max();
    let (home_width, away_width, tip_width) = (
        home_width.unwrap_or(0),
        away_width.unwrap_or(0),
        tip_width.unwrap_or(0),
    );

    let indent = if grouped { "    " } else { "  " };
    let mut current = None;
    for (t, tip) in sorted.iter().zip(&tip_texts) {
        if grouped && current != Some(t.matchday) {
            let _ = writeln!(out, "  Spieltag {}:", t.matchday);
            current = Some(t.matchday);
        }
        let _ = write!(
            out,
            "{indent}{} – {}  {}  (EP {:.3})",
            pad_right(&t.home, home_width),
            pad_right(&t.away, away_width),
            pad_left(tip, tip_width),
            t.expected_points
        );
        if let Some(prev) = t.previous {
            let _ = write!(out, "  bisher {prev}");
        }
        out.push('\n');
    }
}

fn pad_right(s: &str, width: usize) -> String {
    let fill = width.saturating_sub(s.chars().count());
    format!("{s}{}", " ".repeat(fill))
}

fn pad_left(s: &str, width: usize) -> String {
    let fill = width.saturating_sub(s.chars().count());
    format!("{}{s}", " ".repeat(fill))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::Tip;
    use crate::rules::Violation;
    use crate::runner::TenantWarning;

    fn tip(matchday: u32, home: &str, away: &str, h: u8, a: u8, ep: f64) -> ReportedTip {
        ReportedTip {
            matchday,
            home: home.into(),
            away: away.into(),
            tip: Tip { home: h, away: a },
            expected_points: ep,
            previous: None,
        }
    }

    fn tip_prev(
        matchday: u32,
        home: &str,
        away: &str,
        (h, a): (u8, u8),
        ep: f64,
        (ph, pa): (u8, u8),
    ) -> ReportedTip {
        ReportedTip {
            previous: Some(Tip { home: ph, away: pa }),
            ..tip(matchday, home, away, h, a, ep)
        }
    }

    fn report(matchdays: Vec<u32>, tips: Vec<ReportedTip>) -> TenantReport {
        TenantReport {
            matches_open: tips.len(),
            submitted: tips.len(),
            skipped: 0,
            violations: 0,
            dry_run: true,
            matchdays,
            tips,
            warnings: Vec::new(),
        }
    }

    #[test]
    fn aligns_by_chars_with_umlauts() {
        let r = report(
            vec![12],
            vec![
                tip(12, "FC Nordstadt", "SV Südhafen", 2, 1, 1.0129),
                tip(12, "TSV Westfeld", "1. FC Ostberg", 10, 1, 0.702),
            ],
        );
        assert_eq!(
            dry_run_table("alice", &Ok(r)),
            "alice – Spieltag 12 (dry run)\n\
             \x20 FC Nordstadt – SV Südhafen     2:1  (EP 1.013)\n\
             \x20 TSV Westfeld – 1. FC Ostberg  10:1  (EP 0.702)\n"
        );
    }

    #[test]
    fn several_matchdays_are_grouped_and_sorted() {
        let r = report(
            vec![10, 12],
            vec![
                tip(12, "FC Nordstadt", "SV Südhafen", 2, 1, 1.0),
                tip(10, "A", "B", 1, 0, 0.5),
                tip(12, "C", "D", 0, 0, 0.25),
            ],
        );
        assert_eq!(
            dry_run_table("alice", &Ok(r)),
            "alice – Spieltage 10, 12 (dry run)\n\
             \x20 Spieltag 10:\n\
             \x20   A            – B            1:0  (EP 0.500)\n\
             \x20 Spieltag 12:\n\
             \x20   FC Nordstadt – SV Südhafen  2:1  (EP 1.000)\n\
             \x20   C            – D            0:0  (EP 0.250)\n"
        );
    }

    #[test]
    fn shows_previous_tip() {
        let r = report(
            vec![12],
            vec![
                tip_prev(12, "FC Nordstadt", "SV Südhafen", (2, 0), 1.0416, (1, 0)),
                tip_prev(12, "TSV Westfeld", "1. FC Ostberg", (10, 1), 0.71, (1, 1)),
                tip_prev(12, "A", "B", (0, 0), 0.5, (10, 2)),
            ],
        );
        assert_eq!(
            dry_run_table("alice", &Ok(r)),
            "alice – Spieltag 12 (dry run)\n\
             \x20 FC Nordstadt – SV Südhafen     2:0  (EP 1.042)  bisher 1:0\n\
             \x20 TSV Westfeld – 1. FC Ostberg  10:1  (EP 0.710)  bisher 1:1\n\
             \x20 A            – B               0:0  (EP 0.500)  bisher 10:2\n"
        );
    }

    #[test]
    fn previous_only_on_some_rows() {
        let r = report(
            vec![12],
            vec![
                tip(12, "A", "B", 2, 1, 1.5),
                tip_prev(12, "C", "D", (1, 1), 0.75, (0, 0)),
            ],
        );
        let table = dry_run_table("alice", &Ok(r));
        assert_eq!(
            table,
            "alice – Spieltag 12 (dry run)\n\
             \x20 A – B  2:1  (EP 1.500)\n\
             \x20 C – D  1:1  (EP 0.750)  bisher 0:0\n"
        );
        assert!(table.lines().all(|l| l == l.trim_end()), "{table}");
    }

    #[test]
    fn grouped_rows_show_previous_tip() {
        let r = report(
            vec![10, 12],
            vec![
                tip_prev(12, "C", "D", (2, 1), 1.0, (1, 1)),
                tip(10, "A", "B", 1, 0, 0.5),
            ],
        );
        assert_eq!(
            dry_run_table("alice", &Ok(r)),
            "alice – Spieltage 10, 12 (dry run)\n\
             \x20 Spieltag 10:\n\
             \x20   A – B  1:0  (EP 0.500)\n\
             \x20 Spieltag 12:\n\
             \x20   C – D  2:1  (EP 1.000)  bisher 1:1\n"
        );
    }

    #[test]
    fn nothing_to_tip() {
        assert_eq!(
            dry_run_table("demo", &Ok(report(vec![], vec![]))),
            "demo (dry run)\n  nichts zu tippen\n"
        );
        assert_eq!(
            dry_run_table("demo", &Ok(report(vec![12], vec![]))),
            "demo – Spieltag 12 (dry run)\n  nichts zu tippen\n"
        );
    }

    #[test]
    fn failed_tenant() {
        assert_eq!(
            dry_run_table("alice", &Err(TenantError::Timeout(300))),
            "alice – Fehler (dry run)\n  timed out after 300s\n"
        );
    }

    #[test]
    fn warnings_follow_rows() {
        let mut r = report(vec![12], vec![tip(12, "A", "B", 2, 1, 1.5)]);
        r.warnings = vec![
            TenantWarning::Skipped {
                home: "C".into(),
                away: "D".into(),
                reason: "keine Quoten".into(),
            },
            TenantWarning::MaxSameResult(Violation {
                matchday: Some(12),
                result: Tip { home: 2, away: 1 },
                count: 7,
                max: 6,
            }),
        ];
        assert_eq!(
            dry_run_table("alice", &Ok(r)),
            "alice – Spieltag 12 (dry run)\n\
             \x20 A – B  2:1  (EP 1.500)\n\
             \x20 Warnung: C - D: nicht getippt (keine Quoten)\n\
             \x20 Warnung: Regel maxSameResult nicht erfüllbar: Spieltag 12, Ergebnis 2:1 7× getippt (erlaubt 6)\n"
        );
    }

    #[test]
    fn warnings_without_tips() {
        let mut r = report(vec![12], vec![]);
        r.warnings = vec![TenantWarning::Skipped {
            home: "C".into(),
            away: "D".into(),
            reason: "keine Quoten".into(),
        }];
        assert_eq!(
            dry_run_table("alice", &Ok(r)),
            "alice – Spieltag 12 (dry run)\n\
             \x20 keine Tipps berechnet\n\
             \x20 Warnung: C - D: nicht getippt (keine Quoten)\n"
        );
    }

    #[test]
    fn render_all_separates_tenants() {
        let outcomes = vec![
            TenantOutcome {
                name: "alice".into(),
                result: Ok(report(vec![], vec![])),
            },
            TenantOutcome {
                name: "bob".into(),
                result: Err(TenantError::Panicked),
            },
        ];
        assert_eq!(
            render_all(&outcomes),
            "alice (dry run)\n  nichts zu tippen\n\
             \n\
             bob – Fehler (dry run)\n  tenant task panicked\n"
        );
        assert_eq!(render_all(&[]), "");
    }
}
