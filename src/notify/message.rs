//! The per-tenant run summary (German). Pure formatting, no I/O.

use std::fmt::Write as _;

use super::{Level, Message};
use crate::rules::Violation;
use crate::runner::{ReportedTip, TenantError, TenantReport, TenantWarning};

/// Builds the summary of one tenant's run. `None` when there is nothing to
/// report: no tip submitted, no warnings and no error (e.g. everything was
/// already tipped), so the twice-weekly runs do not spam.
pub fn summary(
    tenant: &str,
    result: &Result<TenantReport, TenantError>,
    dry_run: bool,
) -> Option<Message> {
    let suffix = if dry_run { " (Testlauf)" } else { "" };
    let report = match result {
        Ok(r) => r,
        Err(e) => {
            return Some(Message {
                level: Level::Error,
                title: format!("betbot {tenant}: Fehler{suffix}"),
                body: format!("Der Lauf ist fehlgeschlagen:\n{e}"),
            });
        }
    };
    if report.tips.is_empty() && report.warnings.is_empty() {
        return None;
    }

    let n = report.tips.len();
    let status = if !report.warnings.is_empty() {
        "Warnungen".to_owned()
    } else if dry_run {
        format!("{} berechnet", count(n, "Tipp", "Tipps"))
    } else {
        format!("{} abgegeben", count(n, "Tipp", "Tipps"))
    };
    let title = match report.matchdays.last() {
        Some(md) => format!("betbot {tenant}: Spieltag {md} – {status}{suffix}"),
        None => format!("betbot {tenant}: {status}{suffix}"),
    };
    let level = if report.warnings.is_empty() {
        Level::Info
    } else {
        Level::Warning
    };

    let mut body = String::new();
    if let Some(heading) = matchday_heading(&report.matchdays) {
        body.push_str(&heading);
        body.push_str("\n\n");
    }
    if dry_run {
        body.push_str("Testlauf: die Tipps wurden nicht abgegeben.\n\n");
    }
    if report.tips.is_empty() {
        body.push_str("Keine Tipps abgegeben.\n");
    } else {
        write_tips(&mut body, &report.tips);
    }
    let kept = report
        .matches_open
        .saturating_sub(report.submitted)
        .saturating_sub(report.skipped);
    if kept > 0 {
        let _ = writeln!(
            body,
            "{} bereits getippt und unverändert.",
            count(kept, "Spiel war", "Spiele waren")
        );
    }
    body.push('\n');
    if report.warnings.is_empty() {
        body.push_str("Alles in Ordnung.");
    } else {
        body.push_str("Warnungen:");
        for w in &report.warnings {
            body.push_str("\n- ");
            body.push_str(&warning_text(w));
        }
    }
    Some(Message { level, title, body })
}

/// "1 Tipp" / "3 Tipps".
fn count(n: usize, one: &str, many: &str) -> String {
    if n == 1 {
        format!("1 {one}")
    } else {
        format!("{n} {many}")
    }
}

/// "Spieltag 12" or "Spieltag 12 (+ Nachholspiele von Spieltag 9, 10)".
fn matchday_heading(matchdays: &[u32]) -> Option<String> {
    let (main, earlier) = matchdays.split_last()?;
    if earlier.is_empty() {
        return Some(format!("Spieltag {main}"));
    }
    let earlier: Vec<String> = earlier.iter().map(u32::to_string).collect();
    Some(format!(
        "Spieltag {main} (+ Nachholspiele von Spieltag {})",
        earlier.join(", ")
    ))
}

/// One line per tip; grouped by matchday if the tips span several.
fn write_tips(body: &mut String, tips: &[ReportedTip]) {
    let mut sorted: Vec<&ReportedTip> = tips.iter().collect();
    sorted.sort_by_key(|t| t.matchday);
    let grouped = sorted.first().map(|t| t.matchday) != sorted.last().map(|t| t.matchday);
    let mut current = None;
    for t in sorted {
        if grouped && current != Some(t.matchday) {
            if current.is_some() {
                body.push('\n');
            }
            let _ = writeln!(body, "Spieltag {}:", t.matchday);
            current = Some(t.matchday);
        }
        let _ = writeln!(body, "{} - {}  {}", t.home, t.away, t.tip);
    }
}

pub(crate) fn warning_text(w: &TenantWarning) -> String {
    match w {
        TenantWarning::Skipped { home, away, reason } => {
            format!("{home} - {away}: nicht getippt ({reason})")
        }
        TenantWarning::MaxSameResult(v) => violation_text(v),
    }
}

fn violation_text(v: &Violation) -> String {
    let scope = match v.matchday {
        Some(md) => format!("Spieltag {md}"),
        None => "Saison".to_owned(),
    };
    format!(
        "Regel maxSameResult nicht erfüllbar: {scope}, Ergebnis {} {}× getippt (erlaubt {})",
        v.result, v.count, v.max
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::Tip;
    use crate::platform::PlatformError;

    fn tip(matchday: u32, home: &str, away: &str, h: u8, a: u8) -> ReportedTip {
        ReportedTip {
            matchday,
            home: home.into(),
            away: away.into(),
            tip: Tip { home: h, away: a },
            expected_points: 0.0,
        }
    }

    fn report(matchdays: Vec<u32>, tips: Vec<ReportedTip>) -> TenantReport {
        TenantReport {
            matches_open: tips.len(),
            submitted: tips.len(),
            skipped: 0,
            violations: 0,
            dry_run: false,
            matchdays,
            tips,
            warnings: Vec::new(),
        }
    }

    #[test]
    fn ok_summary() {
        let mut r = report(
            vec![12],
            vec![
                tip(12, "FC Nordstadt", "SV Südhafen", 2, 1),
                tip(12, "TSV Westfeld", "1. FC Ostberg", 1, 1),
            ],
        );
        r.matches_open = 3;
        let m = summary("alice", &Ok(r), false).unwrap();
        assert_eq!(m.level, Level::Info);
        assert_eq!(m.title, "betbot alice: Spieltag 12 – 2 Tipps abgegeben");
        assert_eq!(
            m.body,
            "Spieltag 12\n\n\
             FC Nordstadt - SV Südhafen  2:1\n\
             TSV Westfeld - 1. FC Ostberg  1:1\n\
             1 Spiel war bereits getippt und unverändert.\n\
             \n\
             Alles in Ordnung."
        );
    }

    #[test]
    fn stragglers_are_grouped() {
        let r = report(
            vec![10, 12],
            vec![tip(12, "C", "D", 2, 0), tip(10, "A", "B", 1, 0)],
        );
        let m = summary("alice", &Ok(r), false).unwrap();
        assert_eq!(m.title, "betbot alice: Spieltag 12 – 2 Tipps abgegeben");
        assert_eq!(
            m.body,
            "Spieltag 12 (+ Nachholspiele von Spieltag 10)\n\n\
             Spieltag 10:\n\
             A - B  1:0\n\
             \n\
             Spieltag 12:\n\
             C - D  2:0\n\
             \n\
             Alles in Ordnung."
        );
    }

    #[test]
    fn warnings_summary() {
        let mut r = report(vec![12], vec![tip(12, "A", "B", 2, 1)]);
        r.matches_open = 2;
        r.skipped = 1;
        r.violations = 1;
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
            TenantWarning::MaxSameResult(Violation {
                matchday: None,
                result: Tip { home: 1, away: 0 },
                count: 12,
                max: 10,
            }),
        ];
        let m = summary("alice", &Ok(r), false).unwrap();
        assert_eq!(m.level, Level::Warning);
        assert_eq!(m.title, "betbot alice: Spieltag 12 – Warnungen");
        assert_eq!(
            m.body,
            "Spieltag 12\n\n\
             A - B  2:1\n\
             \n\
             Warnungen:\n\
             - C - D: nicht getippt (keine Quoten)\n\
             - Regel maxSameResult nicht erfüllbar: Spieltag 12, Ergebnis 2:1 7× getippt (erlaubt 6)\n\
             - Regel maxSameResult nicht erfüllbar: Saison, Ergebnis 1:0 12× getippt (erlaubt 10)"
        );
    }

    #[test]
    fn warnings_without_tips() {
        let mut r = report(vec![12], vec![]);
        r.matches_open = 1;
        r.skipped = 1;
        r.warnings = vec![TenantWarning::Skipped {
            home: "C".into(),
            away: "D".into(),
            reason: "keine Quoten".into(),
        }];
        let m = summary("alice", &Ok(r), false).unwrap();
        assert_eq!(m.level, Level::Warning);
        assert!(m.body.contains("Keine Tipps abgegeben.\n"), "{}", m.body);
    }

    #[test]
    fn nothing_to_do_sends_nothing() {
        assert!(summary("alice", &Ok(report(vec![], vec![])), false).is_none());
        let mut r = report(vec![12], vec![]);
        r.matches_open = 9;
        assert!(summary("alice", &Ok(r), false).is_none());
    }

    #[test]
    fn error_summary() {
        let err = TenantError::Platform(PlatformError::Auth("login rejected".into()));
        let m = summary("alice", &Err(err), false).unwrap();
        assert_eq!(m.level, Level::Error);
        assert_eq!(m.title, "betbot alice: Fehler");
        assert_eq!(
            m.body,
            "Der Lauf ist fehlgeschlagen:\nplatform: authentication failed: login rejected"
        );
        let m = summary("alice", &Err(TenantError::Timeout(300)), true).unwrap();
        assert_eq!(m.title, "betbot alice: Fehler (Testlauf)");
    }

    #[test]
    fn dry_run_summary() {
        let r = TenantReport {
            dry_run: true,
            ..report(vec![12], vec![tip(12, "A", "B", 1, 0)])
        };
        let m = summary("alice", &Ok(r), true).unwrap();
        assert_eq!(
            m.title,
            "betbot alice: Spieltag 12 – 1 Tipp berechnet (Testlauf)"
        );
        assert!(
            m.body.starts_with(
                "Spieltag 12\n\nTestlauf: die Tipps wurden nicht abgegeben.\n\nA - B  1:0\n"
            ),
            "{}",
            m.body
        );
    }
}
