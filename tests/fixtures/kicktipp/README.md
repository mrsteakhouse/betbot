# kicktipp HTML fixtures

Derived from real kicktipp.de `tippabgabe` pages (Bundesliga 2026/27), provided by the user.

IMPORTANT: the bot sees the **server-rendered** HTML. In the browser DOM, kicktipp's JS (`ktable`)
adds classes such as `tr.datarow`, `td.cell.colN` and `table.ktable` — these do NOT exist in the
HTTP response. The server rows look like:
`<tr><td class="nw kicktipp-time">…</td><td class="nw">Home</td><td class="nw">Away</td>
<td class="kicktipp-tippabgabe">…inputs…</td><td class="nw quoten">…odds…</td></tr>`.

Anonymised / trimmed:
- community slug → `testrunde`, `tipperId` → `12345678`, `tippsaisonId` → `1000001`,
  user name → `Tester`, e-mail → `tester@example.invalid`
- ad tracking URLs replaced by `https://ad.example.invalid/...`; head/scripts/navigation reduced

Unchanged: structure of the tip area, CSS classes, form field names, match ids (`spieltippForms[<id>]`),
kickoff strings (empty cell = same kickoff as the row above), team names, odds, tips.

Server-rendered shape (what the parser must handle):
- `tippabgabe-open-md10.html`: open matchday 10, empty tip inputs, bwin 1X2 odds.
- `tippabgabe-saved-md10.html`: same matchday with two saved tips as `value` attributes
  (1644299506 → 2:0, 1644299500 → 1:2), neobet odds plus a `.tippabgabe-quotenboost` element
  (boosted odd 9.72, must NOT be parsed as 1X2 odds). Structure verified against the real page source.
- `tippabgabe-closed-md9.html`: finished matchday 9, tips read-only in `td.nichttippbar`
  (no match ids). 3× 2:1 and 3× 1:2 → 6/9 mirrored "same result". (Closed-row server markup is
  inferred from the DOM view minus the JS-added classes.)
- `login.html`: real, unmodified logged-out page `https://www.kicktipp.de/info/profil/login`
  (fetched 2026-11-15). Form `#loginForm` POSTs to `/info/profil/loginaction` with `kennung`
  (e-mail) and `passwort`; no hidden inputs / CSRF token.

Browser-DOM shape (robustness only — parser should accept it too):
- `tippabgabe-saved-md10-dom.html`: the saved matchday as copied from the browser dev tools
  (with `datarow` / `cell colN` / `ktable` classes).

Running matchdays: already started matches look like rows of a finished matchday
(`td.nichttippbar`, no inputs, no match id); the remaining matches still have inputs. The mixed state
is derived in tests from `tippabgabe-open-md10.html`.
