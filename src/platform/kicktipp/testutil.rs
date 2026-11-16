//! Fixtures and HTML helpers for kicktipp tests.

pub fn open_md10() -> &'static str {
    include_str!("../../../tests/fixtures/kicktipp/tippabgabe-open-md10.html")
}

pub fn saved_md10() -> &'static str {
    include_str!("../../../tests/fixtures/kicktipp/tippabgabe-saved-md10.html")
}

pub fn saved_md10_dom() -> &'static str {
    include_str!("../../../tests/fixtures/kicktipp/tippabgabe-saved-md10-dom.html")
}

pub fn md9() -> &'static str {
    include_str!("../../../tests/fixtures/kicktipp/tippabgabe-closed-md9.html")
}

pub fn login_page() -> &'static str {
    include_str!("../../../tests/fixtures/kicktipp/login.html")
}

fn replace_once(html: &str, from: &str, to: &str) -> String {
    assert_eq!(html.matches(from).count(), 1, "pattern not unique: {from}");
    html.replacen(from, to, 1)
}

/// Sets the stored tip of an open row the way the real saved page does
/// (`value` attributes on the inputs).
pub fn prefill(html: &str, id: &str, home: &str, away: &str) -> String {
    let input = |field: &str| {
        format!(
            r#"name="spieltippForms[{id}].{field}" type="text" size="2" maxlength="3" autocomplete="off" inputmode="numeric""#
        )
    };
    let heim = input("heimTipp");
    let gast = input("gastTipp");
    let html = replace_once(html, &heim, &format!(r#"{heim} value="{home}""#));
    replace_once(&html, &gast, &format!(r#"{gast} value="{away}""#))
}

/// Turns an open row into a started one (`td.nichttippbar` with `text`).
pub fn close_row(html: &str, id: &str, text: &str) -> String {
    let start = format!(
        r#"<td class="kicktipp-tippabgabe"><input type="hidden" value="true" id="spieltippForms_{id}_tippAbgegeben""#
    );
    let begin = html.find(&start).unwrap();
    let end = begin + html[begin..].find("</td>").unwrap() + "</td>".len();
    format!(
        r#"{}<td class="nichttippbar">{text}</td>{}"#,
        &html[..begin],
        &html[end..]
    )
}

/// Relabels a page from matchday `from` to `to` (title and hidden index).
pub fn as_matchday(html: &str, from: u32, to: u32) -> String {
    let html = replace_once(
        html,
        &format!("<a>{from}. Spieltag</a>"),
        &format!("<a>{to}. Spieltag</a>"),
    );
    replace_once(
        &html,
        &format!(r#"name="spieltagIndex" value="{from}""#),
        &format!(r#"name="spieltagIndex" value="{to}""#),
    )
}

/// Matchday 10 while running: the first two matches have started (tips 2:0 and 1:2).
pub fn mixed_md10() -> String {
    mixed(open_md10())
}

/// Like [`mixed_md10`], based on the saved page (open rows keep their stored tips).
pub fn mixed_saved_md10() -> String {
    mixed(saved_md10())
}

fn mixed(html: &str) -> String {
    let html = close_row(html, "1644299506", "2:0");
    close_row(&html, "1644299500", "1:2")
}
