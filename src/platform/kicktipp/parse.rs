//! Parsing of kicktipp pages and building of the tip form submission.
//! Pure and synchronous (no I/O). Error messages never contain page content.

use std::collections::{HashMap, HashSet};

use jiff::Timestamp;
use scraper::{ElementRef, Html, Selector};
use thiserror::Error;

use crate::domain::{Match, MatchId, Odds, OpenMatch, PlacedTip, Tip, TipSubmission};
use crate::platform::PlatformError;

#[derive(Debug, PartialEq, Eq, Error)]
pub enum ParseError {
    #[error("login required (login form returned instead of the tip page)")]
    LoginRequired,
    #[error("{0}")]
    Unexpected(String),
}

impl From<ParseError> for PlatformError {
    fn from(e: ParseError) -> Self {
        match e {
            ParseError::LoginRequired => PlatformError::Auth(
                "kicktipp did not accept the session (login page returned)".into(),
            ),
            ParseError::Unexpected(m) => PlatformError::UnexpectedPage(m),
        }
    }
}

fn unexpected(msg: impl Into<String>) -> ParseError {
    ParseError::Unexpected(msg.into())
}

/// The tip submission page (`/<community>/tippabgabe`) of one matchday.
#[derive(Debug, Clone)]
pub struct TipPage {
    pub matchday: u32,
    pub matchday_count: u32,
    pub form: FormMeta,
    pub rows: Vec<Row>,
    pub same_result_limit: Option<SameResultLimit>,
}

/// The tip form: where it posts to and its hidden inputs (except the per-match ones).
#[derive(Debug, Clone)]
pub struct FormMeta {
    pub action: String,
    pub hidden: Vec<(String, String)>,
}

/// kicktipp's "Strategieverbot Ergebnis" notice: at most `max` of `of` tips per
/// matchday with the same result.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SameResultLimit {
    pub max: u32,
    pub of: u32,
}

/// One match row of the tip table.
#[derive(Debug, Clone)]
pub struct Row {
    /// Position in the table (0-based).
    pub index: usize,
    pub kickoff: Timestamp,
    pub home: String,
    pub away: String,
    pub odds: Option<Odds>,
    pub state: RowState,
}

#[derive(Debug, Clone)]
pub enum RowState {
    /// Still open: the inputs carry the match id and the raw stored values.
    Open {
        id: MatchId,
        home_value: String,
        away_value: String,
    },
    /// Started or finished: the tip is shown read-only, there is no match id.
    Closed { tip: Option<Tip> },
}

impl Row {
    pub fn tip(&self) -> Option<Tip> {
        match &self.state {
            RowState::Open {
                home_value,
                away_value,
                ..
            } => Some(Tip {
                home: home_value.parse().ok()?,
                away: away_value.parse().ok()?,
            }),
            RowState::Closed { tip } => *tip,
        }
    }

    fn open_id(&self) -> Option<&MatchId> {
        match &self.state {
            RowState::Open { id, .. } => Some(id),
            RowState::Closed { .. } => None,
        }
    }

    fn raw_values(&self) -> Option<(&str, &str)> {
        match &self.state {
            RowState::Open {
                home_value,
                away_value,
                ..
            } => Some((home_value, away_value)),
            RowState::Closed { .. } => None,
        }
    }
}

impl TipPage {
    pub fn hidden(&self, name: &str) -> Option<&str> {
        self.form
            .hidden
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, v)| v.as_str())
    }

    fn open_row(&self, id: &MatchId) -> Option<&Row> {
        self.rows.iter().find(|r| r.open_id() == Some(id))
    }

    pub fn has_open_rows(&self) -> bool {
        self.rows.iter().any(|r| r.open_id().is_some())
    }

    /// Open rows as matches with the user's current tip.
    pub fn open_matches(&self, competition: &str) -> Vec<OpenMatch> {
        self.rows
            .iter()
            .filter_map(|r| {
                let id = r.open_id()?;
                Some(OpenMatch {
                    fixture: Match {
                        id: id.clone(),
                        home: r.home.clone(),
                        away: r.away.clone(),
                        kickoff: r.kickoff,
                        matchday: self.matchday,
                        competition: competition.to_owned(),
                    },
                    tip: r.tip(),
                })
            })
            .collect()
    }

    /// Every row with a tip. Closed rows have no match id on the page and get
    /// the synthetic id `md<matchday>-row<index>` (never all digits, so it
    /// cannot collide with a real id).
    ///
    /// Started rows of the current (mixed) matchday MUST stay in here: they
    /// count for the per-matchday same-result rule.
    pub fn placed_tips(&self) -> Vec<PlacedTip> {
        self.rows
            .iter()
            .filter_map(|r| {
                let tip = r.tip()?;
                let match_id = match r.open_id() {
                    Some(id) => id.clone(),
                    None => MatchId::new(format!("md{}-row{}", self.matchday, r.index)),
                };
                Some(PlacedTip {
                    matchday: self.matchday,
                    match_id,
                    tip,
                })
            })
            .collect()
    }

    /// Odds shown for the wanted (open) matches.
    pub fn odds_for(&self, wanted: &[Match]) -> HashMap<MatchId, Odds> {
        wanted
            .iter()
            .filter_map(|m| {
                let odds = self.open_row(&m.id)?.odds.clone()?;
                Some((m.id.clone(), odds))
            })
            .collect()
    }
}

fn selector(s: &str) -> Result<Selector, ParseError> {
    Selector::parse(s).map_err(|_| unexpected(format!("internal error: invalid selector {s}")))
}

fn text_of(el: ElementRef<'_>) -> String {
    el.text().collect::<String>().trim().to_owned()
}

/// Parses "10. Spieltag" into 10.
fn matchday_label(text: &str) -> Option<u32> {
    let (n, rest) = text.trim().split_once('.')?;
    if rest.trim() != "Spieltag" {
        return None;
    }
    n.trim().parse().ok()
}

pub fn parse_tip_page(html: &str) -> Result<TipPage, ParseError> {
    let doc = Html::parse_document(html);
    let Some(form) = doc.select(&selector("form#tippabgabeForm")?).next() else {
        let login = selector("input[name=kennung], input[name=passwort]")?;
        return Err(if doc.select(&login).next().is_some() {
            ParseError::LoginRequired
        } else {
            unexpected("tip form not found")
        });
    };

    let title = doc
        .select(&selector(".prevnextTitle a")?)
        .next()
        .ok_or_else(|| unexpected("matchday title not found"))?;
    let matchday =
        matchday_label(&text_of(title)).ok_or_else(|| unexpected("unparsable matchday title"))?;

    let action = form
        .value()
        .attr("action")
        .ok_or_else(|| unexpected("tip form has no action"))?
        .to_owned();
    let hidden: Vec<(String, String)> = form
        .select(&selector("input[type=hidden][name]")?)
        .filter_map(|i| {
            let name = i.value().attr("name")?;
            if name.starts_with("spieltippForms[") {
                return None;
            }
            Some((
                name.to_owned(),
                i.value().attr("value").unwrap_or_default().to_owned(),
            ))
        })
        .collect();
    let index = hidden
        .iter()
        .find(|(n, _)| n == "spieltagIndex")
        .and_then(|(_, v)| v.trim().parse::<u32>().ok());
    if index != Some(matchday) {
        return Err(unexpected(
            "matchday title does not match the form's spieltagIndex",
        ));
    }

    let matchday_count = doc
        .select(&selector(".dropdowncontent .menu a")?)
        .filter_map(|a| matchday_label(&text_of(a)))
        .max()
        .unwrap_or(matchday);
    if matchday_count < matchday {
        return Err(unexpected("matchday menu ends before the current matchday"));
    }

    let table = form
        .select(&selector("table#tippabgabeSpiele")?)
        .next()
        .ok_or_else(|| unexpected("tip table not found"))?;
    let cells = RowSelectors::new()?;
    let tr_sel = selector("tbody > tr")?;
    let mut rows = Vec::new();
    let mut previous_kickoff = None;
    // Header rows contain only `th`; they have no `td` children and are skipped.
    let data_rows = table.select(&tr_sel).filter(|tr| !tds(*tr).is_empty());
    for (index, tr) in data_rows.enumerate() {
        let row = parse_row(index, tr, previous_kickoff, &cells)?;
        previous_kickoff = Some(row.kickoff);
        rows.push(row);
    }
    if rows.is_empty() {
        // A Bundesliga matchday is never empty: the page layout must have changed.
        return Err(unexpected("no match rows found"));
    }

    let same_result_limit = doc
        .select(&selector(".messagebox.notice p")?)
        .map(text_of)
        .find(|t| t.contains("Strategieverbot Ergebnis"))
        .and_then(|t| parse_same_result_limit(&t));

    Ok(TipPage {
        matchday,
        matchday_count,
        form: FormMeta { action, hidden },
        rows,
        same_result_limit,
    })
}

fn parse_same_result_limit(text: &str) -> Option<SameResultLimit> {
    let (_, rest) = text.split_once("max.")?;
    let (max, of) = rest.split_whitespace().next()?.split_once('/')?;
    Some(SameResultLimit {
        max: max.parse().ok()?,
        of: of.parse().ok()?,
    })
}

struct RowSelectors {
    heim_input: Selector,
    gast_input: Selector,
    odd_home: Selector,
    odd_draw: Selector,
    odd_away: Selector,
}

impl RowSelectors {
    fn new() -> Result<Self, ParseError> {
        Ok(Self {
            heim_input: selector(r#"input[name$=".heimTipp"]"#)?,
            gast_input: selector(r#"input[name$=".gastTipp"]"#)?,
            odd_home: selector(".quote-heim .quote-text")?,
            odd_draw: selector(".quote-remis .quote-text")?,
            odd_away: selector(".quote-gast .quote-text")?,
        })
    }
}

fn cell<'a>(tr: ElementRef<'a>, sel: &Selector) -> Option<ElementRef<'a>> {
    tr.select(sel).next()
}

/// Direct `td` children of a row. Cells are addressed by position because the
/// browser-only classes (`colN`, `datarow`) are not in the server response.
fn tds(tr: ElementRef<'_>) -> Vec<ElementRef<'_>> {
    tr.children()
        .filter_map(ElementRef::wrap)
        .filter(|e| e.value().name() == "td")
        .collect()
}

fn parse_row(
    index: usize,
    tr: ElementRef<'_>,
    previous_kickoff: Option<Timestamp>,
    s: &RowSelectors,
) -> Result<Row, ParseError> {
    let err = |what: &str| unexpected(format!("row {index}: {what}"));
    let cols = tds(tr);
    let [kickoff_cell, home_cell, away_cell, tip_cell, ..] = cols.as_slice() else {
        return Err(err("unexpected number of cells"));
    };

    let kickoff_text = text_of(*kickoff_cell);
    let kickoff = if kickoff_text.is_empty() {
        previous_kickoff.ok_or_else(|| err("first row has no kickoff"))?
    } else {
        parse_kickoff(&kickoff_text).map_err(|_| err("unparsable kickoff"))?
    };

    let team = |c: ElementRef<'_>, what: &str| {
        Some(text_of(c))
            .filter(|t| !t.is_empty())
            .ok_or_else(|| err(what))
    };
    let home = team(*home_cell, "home team missing")?;
    let away = team(*away_cell, "away team missing")?;

    let tip_cell = *tip_cell;
    let has_class = |c: &str| tip_cell.value().classes().any(|x| x == c);
    let state = if has_class("kicktipp-tippabgabe") {
        let input = |sel: &Selector, suffix: &str| -> Result<(String, String), ParseError> {
            let el = cell(tip_cell, sel).ok_or_else(|| err("tip input missing"))?;
            let name = el.value().attr("name").unwrap_or_default();
            let id = name
                .strip_prefix("spieltippForms[")
                .and_then(|n| n.strip_suffix(suffix))
                .filter(|id| !id.is_empty() && id.bytes().all(|b| b.is_ascii_digit()))
                .ok_or_else(|| err("unparsable match id"))?;
            Ok((
                id.to_owned(),
                el.value().attr("value").unwrap_or_default().to_owned(),
            ))
        };
        let (home_id, home_value) = input(&s.heim_input, "].heimTipp")?;
        let (away_id, away_value) = input(&s.gast_input, "].gastTipp")?;
        if home_id != away_id {
            return Err(err("tip inputs belong to different matches"));
        }
        RowState::Open {
            id: MatchId::new(home_id),
            home_value,
            away_value,
        }
    } else if has_class("nichttippbar") {
        let text = text_of(tip_cell);
        let tip = match text.split_whitespace().next() {
            None => None,
            Some(t) if t.contains('-') => None,
            Some(t) => Some(parse_tip(t).ok_or_else(|| err("unparsable tip"))?),
        };
        RowState::Closed { tip }
    } else {
        return Err(err("unknown tip cell"));
    };

    let odd = |sel: &Selector| -> Option<f64> {
        let t = cell(tr, sel).map(text_of)?;
        t.replace(',', ".").parse().ok()
    };
    let odds = match (odd(&s.odd_home), odd(&s.odd_draw), odd(&s.odd_away)) {
        (Some(home), Some(draw), Some(away)) => Some(Odds {
            home,
            draw,
            away,
            over_under_2_5: None,
        }),
        _ => None,
    };

    Ok(Row {
        index,
        kickoff,
        home,
        away,
        odds,
        state,
    })
}

fn parse_tip(s: &str) -> Option<Tip> {
    let (h, a) = s.split_once(':')?;
    Some(Tip {
        home: h.parse().ok()?,
        away: a.parse().ok()?,
    })
}

/// Parses kicktipp's kickoff format `dd.mm.yy HH:MM` (German local time).
pub fn parse_kickoff(s: &str) -> Result<Timestamp, ParseError> {
    let bad = || unexpected("unparsable kickoff");
    let parts: Vec<&str> = s
        .split(|c: char| c.is_whitespace() || c == '.' || c == ':')
        .filter(|p| !p.is_empty())
        .collect();
    let [day, month, year, hour, minute] = parts.as_slice() else {
        return Err(bad());
    };
    let num = |p: &str| p.parse::<i16>().map_err(|_| bad());
    let mut year = num(year)?;
    if year < 100 {
        year += 2000;
    }
    let small = |p: &str| p.parse::<i8>().map_err(|_| bad());
    jiff::civil::DateTime::new(
        year,
        small(month)?,
        small(day)?,
        small(hour)?,
        small(minute)?,
        0,
        0,
    )
    .and_then(|dt| dt.in_tz("Europe/Berlin"))
    .map(|z| z.timestamp())
    .map_err(|_| bad())
}

/// The login form: where it posts to and its hidden inputs.
#[derive(Debug, Clone)]
pub struct LoginForm {
    pub action: Option<String>,
    pub hidden: Vec<(String, String)>,
}

pub fn parse_login_form(html: &str) -> Result<LoginForm, ParseError> {
    let doc = Html::parse_document(html);
    let kennung = selector("input[name=kennung]")?;
    let form = doc
        .select(&selector("form")?)
        .find(|f| f.select(&kennung).next().is_some())
        .ok_or_else(|| unexpected("login form not found"))?;
    let hidden = form
        .select(&selector("input[type=hidden][name]")?)
        .filter_map(|i| {
            let name = i.value().attr("name")?;
            if name == "kennung" || name == "passwort" {
                return None;
            }
            Some((
                name.to_owned(),
                i.value().attr("value").unwrap_or_default().to_owned(),
            ))
        })
        .collect();
    Ok(LoginForm {
        action: form.value().attr("action").map(str::to_owned),
        hidden,
    })
}

/// The form fields to POST, which tips they set and which were not open.
#[derive(Debug, Clone)]
pub struct Submission {
    pub fields: Vec<(String, String)>,
    pub applied: Vec<TipSubmission>,
    pub skipped: Vec<MatchId>,
}

/// Builds the complete tip form. Every open row is sent (kicktipp treats the
/// form as a whole): rows not in `tips` re-send their stored raw values
/// unchanged. Closed rows and requested matches that are not open are never sent.
pub fn build_submission(
    page: &TipPage,
    tips: &[TipSubmission],
) -> Result<Submission, PlatformError> {
    let mut wanted: HashMap<&MatchId, Tip> = HashMap::new();
    for t in tips {
        if wanted.insert(&t.match_id, t.tip).is_some() {
            return Err(PlatformError::Rejected(format!(
                "duplicate tip for match {}",
                t.match_id
            )));
        }
    }

    let mut fields = page.form.hidden.clone();
    let mut applied = Vec::new();
    for row in &page.rows {
        let RowState::Open {
            id,
            home_value,
            away_value,
        } = &row.state
        else {
            continue;
        };
        let (home, away) = match wanted.get(id) {
            Some(tip) => {
                applied.push(TipSubmission {
                    match_id: id.clone(),
                    tip: *tip,
                });
                (tip.home.to_string(), tip.away.to_string())
            }
            None => (home_value.clone(), away_value.clone()),
        };
        fields.push((format!("spieltippForms[{id}].tippAbgegeben"), "true".into()));
        fields.push((format!("spieltippForms[{id}].heimTipp"), home));
        fields.push((format!("spieltippForms[{id}].gastTipp"), away));
    }
    fields.push(("submitbutton".into(), String::new()));

    let applied_ids: HashSet<&MatchId> = applied.iter().map(|a| &a.match_id).collect();
    let skipped: Vec<MatchId> = tips
        .iter()
        .filter(|t| !applied_ids.contains(&t.match_id))
        .map(|t| t.match_id.clone())
        .collect();
    if applied.is_empty() {
        return Err(PlatformError::Rejected(
            "none of the tips is for a currently open match".into(),
        ));
    }
    Ok(Submission {
        fields,
        applied,
        skipped,
    })
}

/// Checks the page after submitting: every applied tip is stored exactly and
/// every other open row is unchanged.
pub fn verify_stored(
    before: &TipPage,
    after: &TipPage,
    applied: &[TipSubmission],
) -> Result<(), PlatformError> {
    let mut failed: Vec<&MatchId> = Vec::new();
    for a in applied {
        let home = a.tip.home.to_string();
        let away = a.tip.away.to_string();
        let stored = after
            .open_row(&a.match_id)
            .and_then(Row::raw_values)
            .is_some_and(|(h, w)| h == home && w == away);
        if !stored {
            failed.push(&a.match_id);
        }
    }
    let applied_ids: HashSet<&MatchId> = applied.iter().map(|a| &a.match_id).collect();
    for row in &before.rows {
        let (Some(id), Some(raw)) = (row.open_id(), row.raw_values()) else {
            continue;
        };
        if applied_ids.contains(id) {
            continue;
        }
        // Rows that closed in the meantime are not ours to check.
        if let Some(now) = after.open_row(id).and_then(Row::raw_values)
            && now != raw
        {
            failed.push(id);
        }
    }
    if failed.is_empty() {
        return Ok(());
    }
    let ids: Vec<String> = failed.iter().map(|id| id.to_string()).collect();
    Err(PlatformError::Rejected(format!(
        "kicktipp did not store {} tip(s): {}",
        failed.len(),
        ids.join(", ")
    )))
}

#[cfg(test)]
mod tests {
    use super::super::testutil::*;
    use super::*;

    fn ts(s: &str) -> Timestamp {
        s.parse().unwrap()
    }

    fn t(home: u8, away: u8) -> Tip {
        Tip { home, away }
    }

    fn sub(id: &str, tip: Tip) -> TipSubmission {
        TipSubmission {
            match_id: MatchId::new(id),
            tip,
        }
    }

    fn page(html: &str) -> TipPage {
        parse_tip_page(html).unwrap()
    }

    fn odds_of(row: &Row) -> (f64, f64, f64) {
        let o = row.odds.as_ref().unwrap();
        assert!(o.over_under_2_5.is_none());
        (o.home, o.draw, o.away)
    }

    fn ids(p: &TipPage) -> Vec<String> {
        p.rows
            .iter()
            .filter_map(|r| r.open_id().map(ToString::to_string))
            .collect()
    }

    fn raw<'a>(p: &'a TipPage, id: &str) -> (&'a str, &'a str) {
        p.open_row(&MatchId::new(id))
            .and_then(Row::raw_values)
            .unwrap()
    }

    fn field<'a>(s: &'a Submission, name: &str) -> &'a str {
        s.fields
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, v)| v.as_str())
            .unwrap()
    }

    fn assert_unexpected(html: &str) {
        match parse_tip_page(html) {
            Err(ParseError::Unexpected(m)) => assert!(!m.contains('<'), "{m}"),
            other => panic!("expected Unexpected, got {other:?}"),
        }
    }

    const MD10_IDS: [&str; 9] = [
        "1644299506",
        "1644299500",
        "1644299505",
        "1644299507",
        "1644299501",
        "1644299502",
        "1644299503",
        "1644299504",
        "1644299499",
    ];

    #[test]
    fn md10_structure() {
        let p = page(open_md10());
        assert_eq!(p.matchday, 10);
        assert_eq!(p.matchday_count, 34);
        assert_eq!(p.rows.len(), 9);
        assert!(
            p.rows
                .iter()
                .all(|r| matches!(r.state, RowState::Open { .. }))
        );
        assert_eq!(p.form.action, "/testrunde/tippabgabe");
        let hidden: Vec<(&str, &str)> = p
            .form
            .hidden
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect();
        assert_eq!(
            hidden,
            [
                ("tipperId", "12345678"),
                ("spieltagIndex", "10"),
                ("bonus", "false"),
                ("tippsaisonId", "1000001")
            ]
        );
        assert_eq!(p.hidden("tippsaisonId"), Some("1000001"));
        assert_eq!(p.same_result_limit, Some(SameResultLimit { max: 6, of: 9 }));
    }

    #[test]
    fn md10_first_row() {
        let p = page(open_md10());
        let r = &p.rows[0];
        assert_eq!(r.open_id(), Some(&MatchId::new("1644299506")));
        assert_eq!(r.home, "FC Bayern München");
        assert_eq!(r.away, "1. FC Köln");
        assert_eq!(r.kickoff, ts("2026-11-20T19:30:00Z"));
        assert_eq!(odds_of(r), (1.10, 12.0, 21.0));
        assert_eq!(r.tip(), None);
    }

    #[test]
    fn md10_kickoff_carry_forward() {
        let p = page(open_md10());
        let r = &p.rows[2];
        assert_eq!(r.home, "Bor. Mönchengladbach");
        assert_eq!(r.away, "Borussia Dortmund");
        assert_eq!(r.kickoff, ts("2026-11-21T14:30:00Z"));
        assert_eq!(odds_of(r), (4.75, 4.33, 1.63));
        assert_eq!(p.rows[6].kickoff, ts("2026-11-21T17:30:00Z"));
        assert_eq!(p.rows[8].kickoff, ts("2026-11-22T16:30:00Z"));
        assert_eq!(ids(&p), MD10_IDS);
    }

    #[test]
    fn md9_closed() {
        let p = page(md9());
        assert_eq!(p.matchday, 9);
        assert_eq!(p.rows.len(), 9);
        let expected = [
            t(1, 0),
            t(2, 1),
            t(1, 2),
            t(1, 2),
            t(2, 1),
            t(2, 1),
            t(1, 2),
            t(1, 0),
            t(0, 1),
        ];
        for (r, e) in p.rows.iter().zip(expected) {
            assert!(matches!(r.state, RowState::Closed { tip: Some(x) } if x == e));
        }
        assert!(p.open_matches("testrunde").is_empty());
        let placed = p.placed_tips();
        assert_eq!(placed.len(), 9);
        for (i, pt) in placed.iter().enumerate() {
            assert_eq!(pt.match_id, MatchId::new(format!("md9-row{i}")));
            assert_eq!(pt.matchday, 9);
            assert_eq!(pt.tip, expected[i]);
            assert!(!pt.match_id.as_str().bytes().all(|b| b.is_ascii_digit()));
        }
        assert_eq!(p.rows[0].kickoff, ts("2026-11-06T19:30:00Z"));
        assert_eq!(p.rows[2].kickoff, ts("2026-11-07T14:30:00Z"));
    }

    #[test]
    fn saved_md10_tips() {
        let p = page(saved_md10());
        assert_eq!(p.rows.len(), 9);
        for r in &p.rows {
            let id = r.open_id().unwrap().to_string();
            match id.as_str() {
                "1644299506" => assert_eq!(r.tip(), Some(t(2, 0))),
                "1644299500" => assert_eq!(r.tip(), Some(t(1, 2))),
                _ => {
                    assert_eq!(r.tip(), None);
                    assert_eq!(r.raw_values(), Some(("", "")));
                }
            }
        }
        assert_eq!(raw(&p, "1644299506"), ("2", "0"));
        assert_eq!(raw(&p, "1644299500"), ("1", "2"));
    }

    #[test]
    fn saved_md10_odds_ignore_boost() {
        let p = page(saved_md10());
        assert_eq!(odds_of(&p.rows[0]), (1.06, 12.0, 24.0));
        assert_eq!(odds_of(&p.rows[1]), (1.25, 6.80, 10.0));
        assert_eq!(odds_of(&p.rows[2]), (4.90, 4.50, 1.62));
        for r in &p.rows {
            let (h, d, a) = odds_of(r);
            assert!(![h, d, a].contains(&9.72));
        }
    }

    #[test]
    fn saved_md10_structure_matches_open() {
        let s = page(saved_md10());
        let o = page(open_md10());
        assert_eq!(s.matchday, o.matchday);
        assert_eq!(s.matchday_count, o.matchday_count);
        assert_eq!(ids(&s), ids(&o));
        let kickoffs = |p: &TipPage| p.rows.iter().map(|r| r.kickoff).collect::<Vec<_>>();
        assert_eq!(kickoffs(&s), kickoffs(&o));
        assert_eq!(s.form.hidden, o.form.hidden);
        assert_eq!(s.form.action, o.form.action);
    }

    #[test]
    fn half_filled_is_none_but_raw_kept() {
        let html = open_md10().replacen(
            r#"name="spieltippForms[1644299505].heimTipp" type="text" size="2" maxlength="3" autocomplete="off" inputmode="numeric""#,
            r#"name="spieltippForms[1644299505].heimTipp" type="text" size="2" maxlength="3" autocomplete="off" inputmode="numeric" value="1""#,
            1,
        );
        let p = page(&html);
        let r = p.open_row(&MatchId::new("1644299505")).unwrap();
        assert_eq!(r.tip(), None);
        assert_eq!(r.raw_values(), Some(("1", "")));
        let s = build_submission(&p, &[sub("1644299506", t(1, 0))]).unwrap();
        assert_eq!(field(&s, "spieltippForms[1644299505].heimTipp"), "1");
        assert_eq!(field(&s, "spieltippForms[1644299505].gastTipp"), "");
    }

    #[test]
    fn mixed_matchday_rows() {
        let p = page(&mixed_md10());
        let o = page(open_md10());
        assert_eq!(p.rows.len(), 9);
        assert!(matches!(p.rows[0].state, RowState::Closed { tip: Some(x) } if x == t(2, 0)));
        assert_eq!(p.rows[0].kickoff, ts("2026-11-20T19:30:00Z"));
        assert!(matches!(p.rows[1].state, RowState::Closed { tip: Some(x) } if x == t(1, 2)));
        assert_eq!(p.rows[1].kickoff, ts("2026-11-21T14:30:00Z"));
        assert_eq!(p.rows[2].kickoff, ts("2026-11-21T14:30:00Z"));
        assert_eq!(ids(&p), MD10_IDS[2..]);
        for (r, ro) in p.rows.iter().zip(&o.rows).skip(2) {
            assert_eq!(r.open_id(), ro.open_id());
            assert_eq!(r.kickoff, ro.kickoff);
            assert_eq!(odds_of(r), odds_of(ro));
        }
    }

    #[test]
    fn mixed_matchday_open_matches_exclude_started() {
        let open = page(&mixed_md10()).open_matches("testrunde");
        assert_eq!(open.len(), 7);
        for m in &open {
            assert_ne!(m.fixture.id, MatchId::new("1644299506"));
            assert_ne!(m.fixture.id, MatchId::new("1644299500"));
            assert_eq!(m.fixture.matchday, 10);
        }
    }

    #[test]
    fn mixed_matchday_started_tips_are_history() {
        let placed = page(&mixed_md10()).placed_tips();
        let pairs: Vec<(String, u32, Tip)> = placed
            .iter()
            .map(|p| (p.match_id.to_string(), p.matchday, p.tip))
            .collect();
        assert_eq!(
            pairs,
            [
                ("md10-row0".to_owned(), 10, t(2, 0)),
                ("md10-row1".to_owned(), 10, t(1, 2))
            ]
        );

        let html = prefill(&mixed_saved_md10(), "1644299505", "1", "1");
        let placed = page(&html).placed_tips();
        let ids: Vec<String> = placed.iter().map(|p| p.match_id.to_string()).collect();
        assert_eq!(ids, ["md10-row0", "md10-row1", "1644299505"]);
        assert_eq!(placed[2].tip, t(1, 1));
    }

    #[test]
    fn synthetic_ids_cannot_collide() {
        let digits = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
        let mut synthetic = page(md9()).placed_tips();
        synthetic.extend(page(&mixed_md10()).placed_tips());
        let real = ids(&page(open_md10()));
        for s in &synthetic {
            assert!(!digits(s.match_id.as_str()), "{}", s.match_id);
            assert!(!real.contains(&s.match_id.to_string()));
        }
        assert!(real.iter().all(|r| digits(r)));
        let again: Vec<MatchId> = page(md9())
            .placed_tips()
            .into_iter()
            .map(|p| p.match_id)
            .collect();
        let first: Vec<MatchId> = synthetic[..9].iter().map(|p| p.match_id.clone()).collect();
        assert_eq!(first, again);
    }

    #[test]
    fn missing_odds_is_none() {
        let anchor = r#"<a class="quote quote-remis" href="https://ad.example.invalid/click?odds" tabindex="-1" target="_blank"><span class="quote-label">X</span><span class="quote-text">4.33</span></a>"#;
        assert_eq!(open_md10().matches(anchor).count(), 1);
        let p = page(&open_md10().replacen(anchor, "", 1));
        assert!(p.rows[2].odds.is_none());
        assert!(
            p.rows
                .iter()
                .enumerate()
                .all(|(i, r)| i == 2 || r.odds.is_some())
        );
        let wanted: Vec<Match> = p
            .open_matches("testrunde")
            .into_iter()
            .map(|m| m.fixture)
            .collect();
        let odds = p.odds_for(&wanted);
        assert_eq!(odds.len(), 8);
        assert!(!odds.contains_key(&MatchId::new("1644299505")));
    }

    #[test]
    fn login_page_is_login_required() {
        assert!(matches!(
            parse_tip_page(login_page()),
            Err(ParseError::LoginRequired)
        ));
    }

    #[test]
    fn garbage_is_unexpected() {
        assert_unexpected("<html></html>");
        assert_unexpected(&open_md10().replacen("<a>10. Spieltag</a>", "<a>11. Spieltag</a>", 1));
        assert_unexpected(&open_md10().replacen(
            r#"<td class="nw kicktipp-time">20.11.26 20:30</td>"#,
            r#"<td class="nw kicktipp-time"></td>"#,
            1,
        ));
        assert_unexpected(&close_row(open_md10(), "1644299505", "2:0").replacen(
            r#"<td class="nichttippbar">"#,
            "<td>",
            1,
        ));
        assert_unexpected(&open_md10().replacen(
            "spieltippForms[1644299506].gastTipp",
            "spieltippForms[1644299500].gastTipp",
            1,
        ));
        assert_unexpected(&close_row(open_md10(), "1644299505", "x:y"));
    }

    #[test]
    fn server_and_dom_shape_parse_identically() {
        let s = page(saved_md10());
        let d = page(saved_md10_dom());
        assert_eq!(
            (s.matchday, s.matchday_count),
            (d.matchday, d.matchday_count)
        );
        assert_eq!(s.form.action, d.form.action);
        assert_eq!(s.form.hidden, d.form.hidden);
        assert_eq!(s.same_result_limit, d.same_result_limit);
        assert_eq!(s.rows.len(), 9);
        assert_eq!(d.rows.len(), 9);
        for (a, b) in s.rows.iter().zip(&d.rows) {
            assert_eq!(a.index, b.index);
            assert_eq!(a.kickoff, b.kickoff);
            assert_eq!((&a.home, &a.away), (&b.home, &b.away));
            assert_eq!(a.open_id(), b.open_id());
            assert_eq!(a.raw_values(), b.raw_values());
            assert_eq!(a.tip(), b.tip());
            assert_eq!(odds_of(a), odds_of(b));
        }
    }

    #[test]
    fn no_match_rows_is_unexpected() {
        let html = open_md10();
        let start = html.find("<tbody>").unwrap() + "<tbody>".len();
        let end = html.find("</tbody>").unwrap();
        let empty = format!("{}{}", &html[..start], &html[end..]);
        match parse_tip_page(&empty) {
            Err(ParseError::Unexpected(m)) => assert_eq!(m, "no match rows found"),
            other => panic!("expected Unexpected, got {other:?}"),
        }
    }

    #[test]
    fn closed_row_without_tip() {
        for text in ["", "-:-", " - "] {
            let p = page(&close_row(open_md10(), "1644299505", text));
            assert!(matches!(p.rows[2].state, RowState::Closed { tip: None }));
            assert_eq!(p.rows[2].tip(), None);
        }
    }

    #[test]
    fn kickoff_parsing() {
        assert_eq!(
            parse_kickoff("20.11.26 20:30").unwrap(),
            ts("2026-11-20T19:30:00Z")
        );
        assert_eq!(
            parse_kickoff("15.08.26 15:30").unwrap(),
            ts("2026-08-15T13:30:00Z")
        );
        for bad in ["31.02.26 10:00", "x", ""] {
            assert!(
                matches!(parse_kickoff(bad), Err(ParseError::Unexpected(_))),
                "{bad}"
            );
        }
    }

    #[test]
    fn same_result_limit_parsing() {
        assert_eq!(
            parse_same_result_limit(
                "Strategieverbot Ergebnis aktiv ( max. 6/9 Tipps mit gleichem Ergebnis )"
            ),
            Some(SameResultLimit { max: 6, of: 9 })
        );
        let p = page(&open_md10().replacen("Strategieverbot Ergebnis", "Etwas anderes", 1));
        assert_eq!(p.same_result_limit, None);
    }

    #[test]
    fn login_form_parsing() {
        let f = parse_login_form(login_page()).unwrap();
        assert_eq!(f.action.as_deref(), Some("/info/profil/loginaction"));
        assert!(f.hidden.is_empty());
        assert!(matches!(
            parse_login_form(open_md10()),
            Err(ParseError::Unexpected(_))
        ));
    }

    #[test]
    fn build_submission_md10() {
        let p = page(open_md10());
        let s = build_submission(
            &p,
            &[sub("1644299506", t(2, 1)), sub("1644299505", t(0, 0))],
        )
        .unwrap();
        assert_eq!(s.fields.len(), 32);
        assert_eq!(s.fields[..4], p.form.hidden[..]);
        assert_eq!(
            s.fields.last(),
            Some(&("submitbutton".to_owned(), String::new()))
        );
        assert_eq!(field(&s, "spieltippForms[1644299506].heimTipp"), "2");
        assert_eq!(field(&s, "spieltippForms[1644299506].gastTipp"), "1");
        assert_eq!(field(&s, "spieltippForms[1644299505].heimTipp"), "0");
        assert_eq!(field(&s, "spieltippForms[1644299505].gastTipp"), "0");
        for id in MD10_IDS {
            assert_eq!(
                field(&s, &format!("spieltippForms[{id}].tippAbgegeben")),
                "true"
            );
            if id != "1644299506" && id != "1644299505" {
                assert_eq!(field(&s, &format!("spieltippForms[{id}].heimTipp")), "");
                assert_eq!(field(&s, &format!("spieltippForms[{id}].gastTipp")), "");
            }
        }
        assert_eq!(s.applied.len(), 2);
        assert!(s.skipped.is_empty());
    }

    #[test]
    fn build_submission_saved_md10() {
        let p = page(saved_md10());
        let s = build_submission(&p, &[sub("1644299505", t(0, 0))]).unwrap();
        assert_eq!(s.fields.len(), 32);
        assert_eq!(field(&s, "spieltippForms[1644299506].heimTipp"), "2");
        assert_eq!(field(&s, "spieltippForms[1644299506].gastTipp"), "0");
        assert_eq!(field(&s, "spieltippForms[1644299500].heimTipp"), "1");
        assert_eq!(field(&s, "spieltippForms[1644299500].gastTipp"), "2");
        assert_eq!(field(&s, "spieltippForms[1644299505].heimTipp"), "0");
    }

    #[test]
    fn mixed_matchday_submission_skips_started() {
        let p = page(&mixed_md10());
        let s = build_submission(
            &p,
            &[sub("1644299506", t(3, 0)), sub("1644299505", t(0, 0))],
        )
        .unwrap();
        assert_eq!(s.skipped, [MatchId::new("1644299506")]);
        assert_eq!(s.applied, [sub("1644299505", t(0, 0))]);
        assert_eq!(s.fields.len(), 26);
        for (name, _) in &s.fields {
            assert!(!name.contains("1644299506"), "{name}");
            assert!(!name.contains("1644299500"), "{name}");
        }

        assert!(matches!(
            build_submission(&p, &[sub("1644299506", t(3, 0))]),
            Err(PlatformError::Rejected(_))
        ));
        assert!(matches!(
            build_submission(
                &p,
                &[sub("1644299505", t(3, 0)), sub("1644299505", t(1, 0))]
            ),
            Err(PlatformError::Rejected(_))
        ));
    }

    fn rejected_message(r: Result<(), PlatformError>) -> String {
        match r {
            Err(PlatformError::Rejected(m)) => {
                assert!(!m.contains('<'), "{m}");
                m
            }
            other => panic!("expected Rejected, got {other:?}"),
        }
    }

    #[test]
    fn verify_stored_ok_and_mismatch() {
        let before = page(open_md10());
        let applied = [sub("1644299506", t(2, 1))];
        let after = page(&prefill(open_md10(), "1644299506", "2", "1"));
        verify_stored(&before, &after, &applied).unwrap();

        // Not stored at all.
        let m = rejected_message(verify_stored(&before, &before, &applied));
        assert!(m.contains("1644299506"), "{m}");
        // Stored differently.
        let wrong = page(&prefill(open_md10(), "1644299506", "1", "2"));
        let m = rejected_message(verify_stored(&before, &wrong, &applied));
        assert!(m.contains("1644299506"), "{m}");
        // Closed in the meantime.
        let closed = page(&close_row(open_md10(), "1644299506", "2:1"));
        let m = rejected_message(verify_stored(&before, &closed, &applied));
        assert!(m.contains("1644299506"), "{m}");
        // An untouched row changed.
        let changed = page(&prefill(
            &prefill(open_md10(), "1644299506", "2", "1"),
            "1644299499",
            "0",
            "0",
        ));
        let m = rejected_message(verify_stored(&before, &changed, &applied));
        assert!(m.contains("1644299499"), "{m}");
        assert!(!m.contains("1644299506"), "{m}");
    }

    #[test]
    fn verify_stored_saved_md10() {
        let p = page(saved_md10());
        let s = build_submission(&p, &[sub("1644299506", t(2, 0))]).unwrap();
        verify_stored(&p, &p, &s.applied).unwrap();
    }

    #[test]
    fn parse_error_maps_to_platform_error() {
        assert!(matches!(
            PlatformError::from(ParseError::LoginRequired),
            PlatformError::Auth(_)
        ));
        assert!(matches!(
            PlatformError::from(unexpected("x")),
            PlatformError::UnexpectedPage(m) if m == "x"
        ));
    }
}
