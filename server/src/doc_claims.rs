//! Claims these documents make about the CODE, checked rather than trusted.
//!
//! WHY THIS EXISTS. `docs/launch-checklist.md` gates a launch, and four of its nine
//! source citations had drifted to unrelated lines: one pointed at a bare `}`, one at
//! a comment about an error body, one at a struct field, one at a function parameter.
//! Every one still READ as a plausible citation - that is the failure mode - so nothing
//! flagged them, and a reader following one lands somewhere else without necessarily
//! noticing.
//!
//! THE REPOSITORY ALREADY KNOWS THE RULE. `tools/alert-check` fails if `alerts.tsv`
//! cites `observability.md:N`, because "a line citation re-points at its neighbour the
//! moment a row is added". That is the same rule. This is the generalisation: the
//! documents an operator acts on must cite code BY NAME.
//!
//! SCOPE, and it is deliberate. `docs/plans/` is EXCLUDED, because those are
//! HISTORICAL records: a plan that says `config.rs:89` was accurate when it was
//! written, and rewriting it would destroy the record of what was believed then. The
//! launch checklist is the opposite - it describes the present and is read as the
//! present - so a stale citation in it is a live defect.
//!
//! A TEST rather than a CI script, because it needs no new tool, no workflow step and
//! no entry in `tools/README.md`. It runs in the suite that already gates every
//! change, which is where a check earns its keep rather than where it is tidy.
//!
//! NO REGULAR EXPRESSION, deliberately. `regex` is not a dependency of this crate and
//! adding one to enforce a punctuation rule would be a poor trade. The pattern below
//! is "a known file extension, a colon, and a digit", which is three lines of
//! scanning.

/// The file extensions these documents cite. A bare word followed by a colon and a
/// digit is prose ("Gate 5:", "Section 3:") and is deliberately not matched.
const CITED_EXTENSIONS: &[&str] = &[
    "rs", "md", "sh", "ts", "tsx", "astro", "py", "tsv", "toml", "yml", "yaml",
];

/// Resolves a document in the repository's docs/ tree.
///
/// ONE definition, through CARGO_MANIFEST_DIR rather than a relative path. A
/// relative path is relative to the test binary's working directory, which is not
/// guaranteed to be the package root - and the first version of the retention check
/// below got the depth wrong and read nothing. That failed loudly, which is the
/// correct outcome, but a second copy of this path in the next test would have
/// failed the same way for a different reason.
fn doc_path(name: &str) -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("docs")
        .join(name)
}

// ===========================================================================
// WRITING A GUARD HERE - and where the rule is NOT enforced, which is deliberate
// ===========================================================================
//
// A GUARD THAT ASSERTS AN ABSENCE NEEDS A SCOPE, and the scope comes from what the
// absence is taken over. Three answers, and only the middle one needs a floor:
//
//   - A CLOSED SET - an enum, a matched `Self`. Scoped by the COMPILER: adding a
//     variant fails the build, which is the strongest guarantee available. Do nothing.
//   - A RECURSIVE WALK. Scoped by your own discipline, so CARRY A FLOOR on the count
//     (`>= 18` for a 30-file crate), set with SLACK. A floor AT the count is a
//     tripwire: it fires when a file is deleted and is silent when the walk stops early.
//   - A SINGLE DIRECTORY READ. Exhaustive by construction - `read_dir` returns every
//     entry or panics - so there is no traversal to get wrong and no floor is needed.
//     The telegram scaffolding guard below is exempt on this basis, not overlooked.
//
// A floor must be BELOW the count, never equal to it, and must not drift loose: the
// schema declares 119 columns, so a column floor of 60 would pass on a scan reading
// half the schema. Re-derive a floor when the scope it guards changes.
//
// WHY THIS IS A COMMENT AND NOT A CHECK. Two attempts to enforce it by reading this
// file's own source both passed against the defect they were written to catch, and the
// mutation is what told them apart:
//
//   1. Searching each guard's body for a floor matched the WORD in a PROSE COMMENT, so
//      changing a floor to the forbidden strict form still passed.
//   2. Stripping comments first did not help, because the body was extracted to the end
//      of the FILE rather than the end of the function - so every guard's body contained
//      every later guard's floors and the check could not fail at all.
//
// Enforcing this properly needs real brace matching, which is a parser rather than a
// matcher, and a rule enforced by a check that is silently vacuous is worse than a
// rule carried as a convention. A comment cannot pass vacuously. If someone writes a
// correct enforcement, the first thing to do with it is the mutation above.
// ===========================================================================

/// The documents an operator acts on, and the ones whose claims are therefore live.
/// Every rs file under the crate's src directory, recursively.
///
/// Sorted, so a finding list is stable between runs - an unsorted walk pops a
/// directory stack and would report the same violation in a different order each
/// time, which makes a diff of two runs unreadable.
fn source_files() -> Vec<std::path::PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src")];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if path.extension().is_some_and(|e| e == "rs") {
                out.push(path);
            }
        }
    }
    out.sort();
    out
}

/// Source with COMMENTS blanked out, so a check about code does not fire on a
/// comment that happens to discuss the thing being checked.
///
/// Blanked rather than deleted, so every remaining line keeps its original number
/// and a finding names a line the reader can open. Block comments become spaces
/// too, for the same reason: a file-level note that mentions a secret is not a
/// leak, and a check that cannot tell the difference gets deleted by whoever
/// touched the file next.
fn strip_comments(src: &str) -> String {
    let mut out = String::with_capacity(src.len());
    let mut in_block = false;
    for line in src.lines() {
        let mut kept = String::new();
        let mut in_line = false;
        let bytes: Vec<char> = line.chars().collect();
        let mut i = 0;
        while i < bytes.len() {
            if in_block {
                if bytes[i] == '*' && i + 1 < bytes.len() && bytes[i + 1] == '/' {
                    in_block = false;
                    i += 2;
                    continue;
                }
                kept.push(if bytes[i] == '\n' { '\n' } else { ' ' });
                i += 1;
                continue;
            }
            if in_line {
                i += 1;
                continue;
            }
            if bytes[i] == '/' && i + 1 < bytes.len() && bytes[i + 1] == '/' {
                in_line = true;
                i += 2;
                continue;
            }
            if bytes[i] == '/' && i + 1 < bytes.len() && bytes[i + 1] == '*' {
                in_block = true;
                i += 2;
                continue;
            }
            kept.push(bytes[i]);
            i += 1;
        }
        out.push_str(&kept);
        out.push('\n');
    }
    out
}

/// The documents whose line citations are checked.
///
/// A DOCUMENT BELONGS HERE BY BEING CITATION-CHECKED, NOT BY BEING OPERATIONAL, and that
/// distinction is the whole content of this comment because it was got wrong here once.
/// An earlier round added four runbooks - `wind-down.md`, `backup-and-restore.md`,
/// `abuse-runbook.md`, `architecture/identity.md` - on the argument that they are "the
/// four most operationally loaded documents". The list above `TRIAGED` was built by
/// planting a citation in `wind-down.md` and watching the guard pass, so the observation
/// was real; the conclusion did not follow. Being read under pressure makes a citation
/// EXPENSIVE TO GET WRONG, which is a reason to check one that exists, not a reason to
/// require that citations exist.
///
/// Each of the four already carried a TRIAGED reason, and the four are still the answer:
/// `backup-and-restore.md` "cites no source lines" (verified: zero citations in it),
/// `architecture/identity.md` makes claims about a SET of call sites that "no single line
/// citation can carry", and the other two are read after the fact rather than acted on by
/// line citation. Adding them here made their reasons UNREACHABLE - the triage loop
/// `continue`s as soon as `OPERATIONAL_DOCS` matches, so the entry is never consulted -
/// which is worse than either list alone: a reader finds two decisions and no way to tell
/// which one is live.
///
/// The same category error is named in the triage test itself, about a frontend spec
/// excluded for a reason ("the behaviours are covered by the website suite") that argues
/// about COVERAGE rather than about whether a citation can mislead. It is a different
/// question, and being well-tested is not a reason.
///
/// So the rule for this list: a document belongs here when a stale citation in it would
/// mislead, and the four above are excluded because they do not cite at all - a state the
/// triage test's SECOND assertion enforces, by failing an excluded document that has a
/// line citation left to go stale unless the line number IS its content.
const OPERATIONAL_DOCS: &[&str] = &[
    "launch-checklist.md",
    "observability.md",
    "failover.md",
    "billing-system.md",
    "error-model.md",
    "realtime.md",
    "ip-tracking.md",
    "architecture.md",
    "support-model.md",
    "admin-surface.md",
    "edge-relay.md",
    "deployment.md",
    // The operator's runbook, and it belongs here for the reason this list gives rather than by
    // resemblance: an operator FOLLOWS this file. Its steps cite `deployment.md`'s rules by name, and
    // its rollback table tells a reader which action to take under pressure - so a citation that
    // drifted would send somebody to the wrong section while they are already in an incident. That is
    // the mislead this list exists to catch, and it costs more in a document that is executed than in
    // one that is read.
    "deploy-runbook.md",
    "local-development.md",
    "data-retention.md",
    "topology.md",
    "server/api-spec.md",
    "website/06-api-keys-and-limits.md",
];

/// Every line-number citation in the documents above, as `(document, line)`.
///
/// The guard on the guard is the caller's: this returns a list rather than asserting,
/// so the test can require it to be EMPTY. A check that filters into an assertion
/// internally can pass over a scan that matched nothing.
fn line_citations() -> Vec<(String, usize, String)> {
    let docs_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("docs");
    let mut found = Vec::new();

    for name in OPERATIONAL_DOCS {
        let path = docs_dir.join(name);
        let text = std::fs::read_to_string(&path).unwrap_or_else(|e| {
            panic!(
                "{} must be readable, or this check passes over nothing: {e}",
                name
            )
        });
        for (index, line) in text.lines().enumerate() {
            if cites_by_line(line) {
                found.push(((*name).to_string(), index + 1, line.trim().to_string()));
            }
        }
    }
    found
}

/// Whether this line cites a file by line number.
fn cites_by_line(line: &str) -> bool {
    for ext in CITED_EXTENSIONS {
        let needle = format!(".{ext}:");
        let mut from = 0;
        while let Some(at) = line[from..].find(&needle) {
            let after = from + at + needle.len();
            if line[after..].starts_with(|c: char| c.is_ascii_digit()) {
                return true;
            }
            from = after;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn operational_docs_cite_code_by_name_and_never_by_line() {
        let citations = line_citations();
        assert!(
            citations.is_empty(),
            "these documents cite source by LINE, which re-points at a neighbour the\n\
\
             moment anything is inserted above it:\n{:#?}\n\n\
             Cite the function, test or section by name. tools/alert-check already\n\
             enforces exactly this rule for alerts.tsv, for the same reason.",
            citations
        );
    }

    /// The PUBLISHED retention promise equals the code that enforces it.
    ///
    /// WHY THIS IS NOT REDUNDANT WITH THE DOC CITATION CHECK. doc_claims keeps the
    /// documents citing code BY NAME rather than by line. It says nothing about the
    /// NUMBERS, and the numbers are the promise: this document is what a customer is
    /// shown, and what an operator reads to know what a sweep will delete. Its own
    /// closing paragraph names the failure - the policy lived in this document and
    /// nothing connected it to the code - and the fix it records is that the sweeps
    /// exist. The sweeps existing is not the same as the sweeps keeping the period
    /// the document states, and a constant edited from 90 to 60 would leave every
    /// test green while the promise quietly became a lie.
    ///
    /// So each row is pinned in BOTH directions: the document must state the period,
    /// and the constant must equal it. Changing either alone fails here.
    ///
    /// 24 months is pinned as 730 days and the approximation is stated rather than
    /// hidden - a month is not 30.3025 days, so the two can never agree for all time,
    /// and a test pretending otherwise would be asserting a fiction. The constant is
    /// the authority for the sweep, the document is the authority for the promise, and
    /// this test is the only thing that makes them one claim.
    #[test]
    fn the_published_retention_periods_are_the_periods_the_sweeps_enforce() {
        // THE NAME IS NARROWER THAN THE CHECK after four rounds of widening it, and
        // deliberately: this asserts that each period APPEARS in the document, not that
        // the right row carries it. The difference is real and the comment below says
        // which mutations the check catches and which it does not.
        let doc = std::fs::read_to_string(doc_path("data-retention.md"))
            .expect("docs/data-retention.md must be readable, or this passes over nothing");
        let ip_doc = std::fs::read_to_string(doc_path("ip-tracking.md"))
            .expect("docs/ip-tracking.md must be readable, or this passes over nothing");

        // Table named in the doc, the period the doc states, the constant enforcing it.
        //
        // THE STATED PERIODS CARRY NO MARKDOWN, and that is deliberate. They used to be
        // written `**90 days**` and `**7 days**` for the rows the document happens to
        // bold, while two other rows were plain — so unbolding a cell for a purely visual
        // reason would have failed this test, and bolding a plain one would have failed it
        // too. A guard that fails on formatting teaches people to ignore it, and the next
        // real drift would go unread. The number and its unit are the claim; the asterisks
        // are how someone chose to draw it.
        let promises: [(&str, &str, i64); 5] = [
            (
                "usage_events",
                "90 days",
                crate::db::USAGE_EVENTS_RETENTION_DAYS,
            ),
            (
                "usage_daily",
                "24 months",
                crate::db::USAGE_DAILY_RETENTION_DAYS,
            ),
            ("sessions", "30 days", crate::db::SESSION_RETENTION_DAYS),
            (
                "key_ip_seen",
                "7 days",
                crate::ip_tracking::SEEN_RETENTION_DAYS,
            ),
            (
                "key_ip_daily",
                "90 days",
                crate::ip_tracking::DAILY_RETENTION_DAYS,
            ),
        ];

        for (table, stated, constant) in promises {
            // IN THE ROW THAT NAMES THE TABLE, not anywhere in the file. The check used
            // to be a document-wide substring search, and the mutation proved why that is
            // not the same thing: changing the per-request row from 90 to 60 days passed,
            // because key_ip_daily also says 90 and the string was still present. A guard
            // that cannot tell two rows apart is checking that a number occurs, not that a
            // promise is made, and the name says it is the latter.
            // IN A ROW THAT NAMES THE TABLE, and that is possible now rather than a
            // hand-kept mapping. It failed two rounds ago because the retention table
            // did not contain the table names at all - it called them "Usage daily" and
            // "Sessions (expired/revoked)", and had no row whatsoever for the two key_ip
            // tables the sweep deletes.
            //
            // The fix was to the DOCUMENT, not to the check: every row now names the
            // table it governs, so a reader can find it in the schema and a test can find
            // the row by name. A retention table that names its tables can be checked
            // against the code; one that does not can only be checked by string
            // coincidence, and this check did exactly that - two rows sharing a number
            // were indistinguishable, so changing the per-request window from 90 to 60
            // days passed because key_ip_daily also said 90.
            //
            // It takes SOME naming row rather than the first, because the document has
            // three tables that mention these names and only the retention one carries a
            // period.
            let promised = doc
                .lines()
                .map(str::trim)
                .filter(|l| l.starts_with('|') && l.contains(table))
                .any(|row| row.contains(stated));
            assert!(
                promised,
                "no table row in docs/data-retention.md naming {table} states {stated}; the constant is {constant} days. A retention period nobody was told about is not a promise, and one table changing its window does not revise another's."
            );
        }

        // The constants, named outright, because a table row that merely CONTAINS
        // the number would also be satisfied by a change to the constant alone.
        assert_eq!(
            crate::db::USAGE_EVENTS_RETENTION_DAYS,
            90,
            "the document promises 90 days of per-request usage"
        );
        assert_eq!(
            crate::db::USAGE_DAILY_RETENTION_DAYS, 730,
            "the document promises 24 months of usage_daily and 730 days is what that resolves to here; if the sweep changes, the promise must be re-read, because two thirds of a year is a different promise"
        );
        assert_eq!(
            crate::db::SESSION_RETENTION_DAYS,
            30,
            "the document promises 30 days of expired sessions"
        );
        assert_eq!(
            crate::ip_tracking::SEEN_RETENTION_DAYS,
            7,
            "the document promises 7 days of key_ip_seen"
        );
        assert_eq!(
            crate::ip_tracking::DAILY_RETENTION_DAYS,
            90,
            "the document promises 90 days of key_ip_daily"
        );

        // THE SAME POLICY IS PUBLISHED A THIRD TIME, and this copy is the one a
        // CUSTOMER reads. website/src/lib/privacy.ts is not a document about the
        // policy - it is the page the site renders from, so a number in it is the
        // number a customer is shown. It said per-request usage was "kept
        // indefinitely" because a purge job was "not yet running", while the
        // nightly scheduler deletes those rows at 90 days. The two internal
        // documents were right; the one a customer reads was the one that was
        // wrong, and no test could see it because it lives in another language.
        let privacy = std::fs::read_to_string(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("..")
                .join("website")
                .join("src")
                .join("lib")
                .join("privacy.ts"),
        )
        .expect("website/src/lib/privacy.ts must be readable, or this passes over nothing");
        assert!(
            privacy.contains("'Per-request usage', keep: '90 days'"),
            "website/src/lib/privacy.ts no longer states 90 days for per-request usage. That \
             array IS the privacy page the site renders, so a number in it is a number a \
             customer is shown."
        );
        assert!(
            !privacy.contains("'Per-request usage', keep: 'kept indefinitely'")
                && !privacy.contains("'Per-request usage', keep: 'indefinitely'"),
            "website/src/lib/privacy.ts tells customers their per-request usage is kept \
             INDEFINITELY because no purge job is running. The nightly maintenance \
             scheduler deletes those rows at 90 days, and two documents in this \
             repository say so. A privacy page that understates collection is still a false \
             statement about data handling, and this one is the statement a customer is \
             held to."
        );
        // WHAT THE CHECK ABOVE USED TO BE, and why it is narrower now. It was
        // `!privacy.contains("kept indefinitely")` - the whole file, any row. That
        // was a proxy for the defect rather than the defect, and it became wrong
        // the moment a DIFFERENT row was honestly unbounded: the Reviews row now
        // says "Kept indefinitely; not deleted on request", which is true (nothing
        // in server/src deletes a review) and was rejected by the guard. A check
        // keyed on a phrase cannot tell a truthful use of the phrase from a false
        // one, so it is keyed on the ROW - `what` and the unbounded `keep` in the
        // same object - which is the thing that can be wrong.

        // THE SAME POLICY IS PUBLISHED TWICE in docs/, and one document being correct is no
        // help when the other says something else. docs/ip-tracking.md carries its own
        // retention table for the same tables. It is the page a customer asking
        // how long their IP is kept is answered from, while data-retention.md is the
        // page an operator reading a sweep is answered from. The two agreeing IS the
        // promise; one of them being right is not.
        //
        // Matched on the table ROW rather than a bare number, because a document that
        // says 90 days somewhere is not the same as one that promises 90 days OF
        // key_ip_daily.
        for (table, stated) in [
            ("key_ip_seen", "| `key_ip_seen` hashes | **7 days**"),
            (
                "link_redemption_attempts",
                "| `link_redemption_attempts` hashes | **7 days**",
            ),
            ("auth_attempts", "| `auth_attempts` rows | **7 days**"),
            ("key_ip_daily", "| `key_ip_daily` counts | 90 days"),
        ] {
            assert!(
                ip_doc.contains(stated),
                "docs/ip-tracking.md no longer states {stated} for {table}. That table and the one in data-retention.md are the same policy published twice, and a promise that differs between the page a customer reads and the page an operator reads is not a promise."
            );
        }
    }

    /// The REGISTER's session lifetime is the config's session lifetime.
    ///
    /// Same class as the retention periods, and a sharper case, because this one is
    /// a SECURITY property and the register is where a reader looks to learn how
    /// long a stolen credential survives. docs/decisions.md states 30 days absolute
    /// and 7 days idle; config/apikita.toml carries the two numbers that decide it.
    ///
    /// There IS a test that couples them, and it is not this one. The session test
    /// ages a session 8 days and expects a refusal, which passes only because the
    /// shipped idle bound is 7. That is a real coupling and a poor statement of it:
    /// it fails with a message about the REGISTER when the CONFIG changed, and it
    /// would fail for the wrong reason if both moved together. This states the claim
    /// directly, so a change to either side names itself.
    ///
    /// Both directions again: the register must say it, and the config must match.
    #[test]
    fn the_register_session_lifetime_is_the_lifetime_the_config_enforces() {
        let register = std::fs::read_to_string(doc_path("decisions.md"))
            .expect("docs/decisions.md must be readable, or this passes over nothing");
        let config =
            crate::config::AppConfig::load_from_file(concat!("../", "config/apikita.toml"))
                .or_else(|_| crate::config::AppConfig::load_from_file("config/apikita.toml"))
                .expect("config/apikita.toml must load");

        assert!(
            register.contains("30 days absolute, 7 days idle"),
            "the decision register no longer states the session lifetime as 30 days absolute and 7 days idle. The config carries {} and {} respectively, so either the register was reworded or the policy changed without it being written down - and a stale register is worse than none, because it is what a reader trusts.",
            config.sessions.absolute_days,
            config.sessions.idle_days
        );

        assert_eq!(
            config.sessions.absolute_days, 30,
            "the register promises a 30-day absolute session lifetime"
        );
        assert_eq!(
            config.sessions.idle_days, 7,
            "the register promises a 7-day idle session lifetime, and that is also the only combination that makes the idle bound bind at all: at or above absolute_days the idle rule is inert by construction"
        );
    }

    /// Every OTHER number the register states is the number the config carries.
    ///
    /// The class the session guard above opened, extended to the rest of the table. A register
    /// row that states a figure which also lives in `config/apikita.toml` is published twice,
    /// and two copies of one number drift - this repository's most-repeated finding, in the
    /// worst possible place, because `docs/decisions.md` calls itself "the single source of
    /// truth for settled decisions". When it disagrees with the config, a reader is told to
    /// trust the register, which is what makes this worse than a cosmetic mismatch.
    ///
    /// MEASURED, which is why this exists rather than being assumed covered. Changing
    /// `credit_expiry_months` from 24 to 36, and `key_metadata_cache_seconds` from 60 to 300,
    /// each left the ENTIRE suite green at 637 passed, while the register went on stating 24
    /// months and 60 seconds. Both mutations were confirmed to have LANDED - the config line
    /// printed before and after - before the verdict was read, because a mutation that does
    /// not apply passes for the wrong reason.
    ///
    /// The session row was guarded and these two were not, which is the shape this repository
    /// keeps producing: one instance fixed where a class existed.
    ///
    /// THE NUMBER IS PARSED OUT OF THE REGISTER, not written here. The first version of this
    /// test hardcoded 24 and 60 beside the config reads, and that is the mistake
    /// `every_model_carries_the_margin_the_decision_record_states` already warns about in its
    /// own doc-comment: "a copy beside the constant is a second place to update, and it goes
    /// stale the way the claim would without it." A literal here would mean the decision
    /// could not be changed without editing the test that exists to let it be changed.
    #[test]
    fn every_other_number_the_register_states_is_the_number_the_config_carries() {
        let register = std::fs::read_to_string(doc_path("decisions.md"))
            .expect("docs/decisions.md must be readable, or this passes over nothing");
        let config =
            crate::config::AppConfig::load_from_file(concat!("../", "config/apikita.toml"))
                .or_else(|_| crate::config::AppConfig::load_from_file("config/apikita.toml"))
                .expect("config/apikita.toml must load");

        // (row label, the UNIT the config key is denominated in, config value, what to call it
        // when reporting).
        //
        // THE ANCHOR IS THE UNIT, NOT THE NUMBER. An earlier version anchored on the literal
        // "24 months", which is the same mistake as hardcoding the figure one line over: a
        // legitimate move to 36 months could not find its own anchor, so the test that exists
        // to ALLOW the decision to change refused to let it. Anchoring on the unit ("months",
        // "seconds") means the row can carry any figure, and the unit word is what ties the
        // figure to the config key's denominator - a row reworded to weeks would fail here
        // rather than silently comparing weeks against months.
        //
        // THE EIGHT LIMIT ROWS WERE TESTED BEFORE BEING ADDED. Mutating each config value and
        // running the full suite left all EIGHT green at 638 passed, with the mutation's
        // before/after value printed to confirm it had landed - so each was genuinely
        // unpinned, not merely suspicious. `min_first_deposit` and `min_topup` were tested the
        // same way and are DELIBERATELY ABSENT: changing either fails two tests already, so a
        // guard here would be a third copy of one rule.
        //
        // Their units are `per`, because these rows state a bare count and the key name
        // carries the denominator (`_per_minute`, `_per_day`). Anchoring on a unit word that
        // is not in the row would fail at the anchor rather than at the number.
        let rows: [(&str, &str, i64, &str); 10] = [
            (
                "Credit expiry",
                "months",
                config.wallet.credit_expiry_months as i64,
                "credit expiry",
            ),
            (
                "Key metadata cache TTL",
                "seconds",
                config.limits.key_metadata_cache_seconds as i64,
                "the key-metadata cache TTL",
            ),
            (
                "Wallet mutation limit",
                "per",
                config.limits.wallet_mutations_per_minute as i64,
                "the wallet mutation limit",
            ),
            (
                "SSE replay buffer",
                "events",
                config.realtime.replay_buffer_events as i64,
                "the SSE replay buffer",
            ),
            (
                "SSE connections per account",
                "**",
                config.realtime.max_connections_per_account as i64,
                "the per-account SSE connection cap",
            ),
            (
                "Health-check interval",
                "failures",
                config.circuit_breaker.health_check_failures as i64,
                "the health-check failure threshold",
            ),
            (
                "Key pool attempts",
                "**",
                config.key_pool.max_key_attempts as i64,
                "the key-pool attempt count",
            ),
            (
                "Key creation",
                "per",
                config.limits.key_creation_per_day as i64,
                "the daily key-creation cap",
            ),
            (
                "Review submissions",
                "per",
                config.limits.review_per_hour as i64,
                "the hourly review cap",
            ),
            (
                "Low-balance DM",
                "Below",
                config.wallet.low_balance_threshold_idr as i64,
                "the low-balance threshold",
            ),
        ];

        for (label, phrase, from_config, about) in rows {
            let row = register
                .lines()
                .find(|l| l.starts_with("| ") && l.contains(label))
                .unwrap_or_else(|| {
                    panic!(
                        "{about}: docs/decisions.md no longer has a `{label}` row. Either the \
                         row was renamed - in which case update this check in the same commit - \
                         or the decision was dropped, and a dropped row is a decision nobody \
                         recorded."
                    )
                });
            let at = row.find(phrase).unwrap_or_else(|| {
                panic!(
                    "{about}: the `{label}` row no longer contains `{phrase}`, so this check \
                     cannot read the figure out of it. If the row states the same decision in \
                     different words, point this at the new words in the same commit. Row was: \
                     {row}"
                )
            });

            // The integer may sit BEFORE the phrase ("**60 seconds**") or INSIDE it ("24
            // months"), so try both and take whichever is present. `first_integer` already
            // ignores digits welded to letters, which is what keeps a promise like
            // "12h" from reading as 12 when the config counts minutes.
            let stated = first_integer(&row[at..])
                .or_else(|| {
                    // The number sits before the phrase, possibly with markdown between: the
                    // row reads `| **60 seconds** |`, so the characters immediately left of
                    // `seconds` are `**`, not digits. Skip the non-digit run first, then take
                    // the digit run.
                    let head = &row[..at];
                    let digits: String = head
                        .chars()
                        .rev()
                        .skip_while(|c| !c.is_ascii_digit())
                        .take_while(|c| c.is_ascii_digit())
                        .collect::<Vec<_>>()
                        .into_iter()
                        .rev()
                        .collect();
                    digits.parse::<i64>().ok()
                })
                .unwrap_or_else(|| {
                    panic!(
                        "{about}: the `{label}` row's `{phrase}` has no integer beside it, but \
                         this check exists to compare one against the config. Row was: {row}"
                    )
                });

            assert_eq!(
                from_config, stated,
                "{about}: the register states {stated} and config/apikita.toml carries \
                 {from_config}. These are one decision published twice, and the register wins \
                 with a reader - change both or neither."
            );
        }
    }

    /// The first integer in a string, ignoring digits that are part of a larger token.
    ///
    /// Used to read a figure out of a register row rather than restating it in the test. Kept
    /// deliberately narrow: no decimals, no signs - a decision row states a plain count.
    ///
    /// COMMAS AND UNDERSCORES ARE PART OF THE NUMBER. The register writes `**Below 10,000 IDR**`
    /// and the config writes `10000`, so a reader that stopped at the comma returned 10 and
    /// reported a 1000x disagreement that does not exist. The separator is only absorbed when a
    /// digit follows it, so `**10, max 1/day**` still reads as 10.
    fn first_integer(s: &str) -> Option<i64> {
        let bytes = s.as_bytes();
        let mut i = 0;
        while i < bytes.len() {
            if bytes[i].is_ascii_digit() && (i == 0 || !bytes[i - 1].is_ascii_alphanumeric()) {
                let start = i;
                while i < bytes.len() {
                    // A separator is absorbed ONLY between digits, so `10,000` is one number
                    // and `10, max` stops at 10.
                    let is_digit = bytes[i].is_ascii_digit();
                    let is_separator_between_digits = (bytes[i] == b',' || bytes[i] == b'_')
                        && i + 1 < bytes.len()
                        && bytes[i + 1].is_ascii_digit();
                    if !is_digit && !is_separator_between_digits {
                        break;
                    }
                    i += 1;
                }
                // A digit run welded to letters is usually a QUANTITY (`30s`, `10k`), which is
                // exactly what a register row states. It is only an identifier when the token
                // is long and snake_case (`20260925000000_initial`), which is what the length
                // and underscore conditions below separate out.
                let tail_start = i;
                let mut j = i;
                while j < bytes.len() && (bytes[j].is_ascii_alphanumeric() || bytes[j] == b'_') {
                    j += 1;
                }
                let tail = &s[tail_start..j];
                if tail.len() > 2 && tail.contains('_') {
                    // part of an identifier like `20260925000000_initial` - keep looking
                    continue;
                }
                let token = s[start..i].replace([',', '_'], "");
                return token.parse::<i64>().ok();
            }
            i += 1;
        }
        None
    }

    /// `first_integer` reads a figure the way the register writes it.
    ///
    /// Written because the comma bug above was found by a test failing on a 1000x "disagreement"
    /// rather than by reading the code, and the next reader deserves the cases spelled out.
    #[test]
    fn the_first_integer_reader_handles_thousands_separators_and_stops_at_prose() {
        for (input, expected) in [
            ("**Below 10,000 IDR, max 1/day**", Some(10_000)),
            ("**2 years (24 months) from each deposit**", Some(2)),
            ("**60 seconds**", Some(60)),
            ("**100 events**", Some(100)),
            ("**5**", Some(5)),
            ("**30s, 2 failures to fail over**", Some(30)),
            ("no digits here", None),
            // a separator that is NOT between digits must not be swallowed
            ("**10, max 1/day**", Some(10)),
        ] {
            assert_eq!(
                first_integer(input),
                expected,
                "first_integer disagreed about: {input}"
            );
        }
    }

    /// The bot README's claim about the FOLDER is true, checked against the tree.
    ///
    /// A third kind of promise, and the other two are the wrong shape for this one.
    /// The retention and lifetime checks tie a DOCUMENT to CODE. This one ties a
    /// document to the REPOSITORY TREE: telegram/README.md states that the folder
    /// holds this README and nothing else.
    ///
    /// THIS GUARD USED TO HAVE A SECOND SUBJECT and no longer does.
    /// docs/launch-checklist.md carried an open item that depended on the same fact -
    /// that the bot was design-only - and that item WAS a launch gate, which made this
    /// the expensive direction to be wrong in. Both items are now closed: the review
    /// flow is served to the website session and the top-up feed's settlement gate is
    /// enforced on SSE, so nothing about a launch hangs on whether the bot exists.
    /// The nudge is retained for the README's own claim.
    ///
    /// What is left still has the property the others do not: it goes STALE by someone
    /// doing ordinary work. Writing the bot is not a defect, it is the next task, and
    /// telegram/README.md would quietly become wrong.
    ///
    /// So the failure is deliberately a NUDGE rather than a veto, and it says what to
    /// do. -Force matters: without it a .gitkeep or an editor swap file would read as
    /// a bot, and the check would cry wolf the first time someone opened the folder.
    ///
    /// THE FILTERING IS THE IMPLEMENTATION, not a note beside it. Measured: the first
    /// version of this test read the directory with no filter at all while this comment
    /// claimed a .gitkeep would be ignored, so dropping a `.gitkeep` in `telegram/` --
    /// the ordinary act this paragraph exists to tolerate -- FAILED the test. A comment
    /// promising tolerance the code does not provide is worse than saying nothing, so
    /// the dot- and editor-suffix rules below are the ones this paragraph describes.
    #[test]
    fn the_telegram_folder_is_still_the_scaffolding_its_readme_claims() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("telegram");

        /// Names an ordinary visit to the folder leaves behind: version-control
        /// placeholders and editor swap/backup files. None of them is a bot.
        fn is_incidental(name: &str) -> bool {
            if name.starts_with('.') {
                return true;
            }
            // vim (.swp/.swo), emacs (#x#, x~), and the usual backup suffixes.
            name.ends_with(".swp")
                || name.ends_with(".swo")
                || name.ends_with('~')
                || (name.starts_with('#') && name.ends_with('#'))
        }

        let mut others: Vec<String> = std::fs::read_dir(&dir)
            .expect("telegram/ must be readable")
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|name| name != "README.md" && !is_incidental(name))
            .collect();
        others.sort();

        assert!(
            others.is_empty(),
            "telegram/ now holds {others:?} besides its README. That is the NEXT TASK rather than a defect, but telegram/README.md states the folder holds this README and nothing else, so it needs updating. Note what is NO LONGER part of this nudge: docs/launch-checklist.md used to carry an open item because the bot was design-only, and that item was a launch gate. It is CLOSED - the bot is formally deferred and the two gates that ran through it are closed by serving the review flow to the website session and by the existing settlement-gated SSE feed. So this now guards the README's claim about the folder and nothing else; building the bot is ordinary work with no launch gate hanging on it."
        );

        // The README is the other half of the claim, and it is worth asserting that the

        // document still EXISTS - a guard that reads a file nobody removed is a guard

        // whose subject has quietly gone.

        let readme = std::fs::read_to_string(dir.join("README.md"))
            .expect("telegram/README.md must exist for the claim above to be about it");

        assert!(
            readme.contains("empty scaffolding"),
            "telegram/README.md no longer describes itself as empty scaffolding. If the

            bot is being built, its Status section is the place that says so - and the

            test above is what will make this line need updating."
        );
    }

    /// The link-code TTL in the CONTRACT is the TTL in the code.
    ///
    /// `routes/telegram.rs` states the 5-minute window in a constant whose own doc-comment says
    /// why it is a constant and not a config knob: "The TTL is NOT configurable: it is a
    /// security parameter stated in the contract, and a config knob would let a deployment widen
    /// it by accident." Two documents state the same figure to readers - `docs/server/api-spec.md`
    /// and `docs/architecture/identity.md`.
    ///
    /// MEASURED, which is why this exists: changing `LINK_CODE_TTL_MINUTES` from 5 to 15 left the
    /// ENTIRE suite green at 641 passed, while both documents went on telling a reader 5 minutes.
    /// The reverse drifts too - rewording `api-spec.md` to "15-minute TTL" also passed.
    ///
    /// WHY IT IS WORTH A GUARD rather than a comment. The TTL is the second line of defence on a
    /// brute-forceable 6-digit code: the code space is 10^6 and the caps bound attempts, but a
    /// LONGER window means more codes are live at once, so an attacker's odds per guess rise
    /// without any counter changing. The constant's own comment says the contract is what fixes
    /// it; nothing enforced that the contract and the constant agree.
    ///
    /// THE FIGURE IS READ FROM THE CONTRACT, NOT WRITTEN HERE, and the first version of this test
    /// got that wrong: it asserted `ttl == 5`, so moving the decision deliberately - the thing a
    /// security parameter exists to allow, under review - failed the guard that was supposed to
    /// permit it. Same mistake as the register guard two tests up, in the same week. The number
    /// comes from `api-spec.md`; this test only insists that the constant and both documents
    /// agree with it.
    #[test]
    fn the_link_code_ttl_in_the_contract_is_the_ttl_in_the_code() {
        let ttl = crate::routes::telegram::LINK_CODE_TTL_MINUTES;

        // `api-spec.md` is the contract, and it states the figure in the sentence that also
        // carries the "single-use" property, so the number cannot be picked up from an unrelated
        // mention.
        let spec = std::fs::read_to_string(doc_path("server/api-spec.md"))
            .expect("docs/server/api-spec.md must be readable, or this passes over nothing");
        let sentence = spec
            .lines()
            .find(|l| l.contains("single-use,") && l.contains("-minute TTL"))
            .unwrap_or_else(|| {
                panic!(
                    "docs/server/api-spec.md no longer states the link-code TTL in a sentence \
                     containing both `single-use,` and `-minute TTL`, so this check cannot read \
                     the figure out of the contract. If the sentence was reworded, point this at \
                     the new wording in the same commit."
                )
            });
        // ANCHOR ON `single-use,`, NOT ON THE LINE START. The sentence reads "Issues a 6-digit
        // code: single-use, 5-minute TTL, ...", so reading the first integer of the LINE returns
        // the 6 from "6-digit" - a code length, not a lifetime. Measured: that is exactly what
        // the first version of this parser did, and it reported the contract promising 6 minutes.
        let after_property = &sentence[sentence.find("single-use,").unwrap_or(0)..];
        let promised = first_integer(after_property).unwrap_or_else(|| {
            panic!("could not read a number out of the contract sentence: {sentence}")
        });

        assert_eq!(
            ttl, promised,
            "routes/telegram.rs carries LINK_CODE_TTL_MINUTES = {ttl} and the contract promises \
             {promised} minutes. The constant's own comment says the TTL is fixed by the contract \
             rather than by a knob, so these two are one decision and must move together."
        );

        // The second document must agree as well, with the figure it actually writes.
        let identity = std::fs::read_to_string(doc_path("architecture/identity.md"))
            .expect("docs/architecture/identity.md must be readable");
        let line = identity
            .lines()
            .find(|l| l.contains("short TTL ("))
            .unwrap_or_else(|| {
                panic!(
                    "docs/architecture/identity.md no longer describes the link-code TTL as a \
                     `short TTL (...)`, so this check cannot read the figure out of it."
                )
            });
        let stated = first_integer(&line[line.find("short TTL (").unwrap_or(0)..])
            .unwrap_or_else(|| panic!("could not read a number out of the identity doc: {line}"));
        assert_eq!(
            stated, ttl,
            "docs/architecture/identity.md tells a reader the code lives {stated} minutes while \
             LINK_CODE_TTL_MINUTES is {ttl}. Both describe the same window on a brute-forceable \
             code, so a reader acting on the document is misled in the direction that matters."
        );
    }

    /// No secret value is ever interpolated into a log or format macro.
    ///
    /// A FOURTH claim checked, and the only one about SOURCE rather than about a
    /// value. website/src/lib/privacy.ts tells customers the Midtrans server key is
    /// never logged, and that is a statement a check can verify: find every log and
    /// format macro in the crate and see whether a secret-bearing identifier is
    /// substituted into one.
    ///
    /// WHY IT MATCHES AN INTERPOLATION AND NOT A MENTION. Much of this crate
    /// discusses secrets in comments - the key is passed to an HMAC, compared,
    /// refused when empty - and a check that flagged the WORD would fire on
    /// documentation and be deleted. The rule is the one that matters: a secret name
    /// appearing where a value would be substituted, which is right after a percent
    /// sign or an opening brace. Comments are stripped first for the same reason.
    ///
    /// It is a measurement rather than a guess: hundreds of macro calls, zero
    /// interpolations. The macro count is ASSERTED below and moves with the code; the
    /// FILE count is deliberately not stated, because a number written beside a claim
    /// about an absence is a number that drifts - this one said 29 files when the crate
    /// held 30, and a reader could reasonably have read that as the guard missing one.
    /// `source_files` walks the whole of src with no exclusions, so the scope is a
    /// property of that function and not of a number here.
    ///
    /// This is what keeps the property: logging a secret is the most ordinary mistake
    /// there is while debugging a failing payment path, and nothing else in this crate
    /// would notice.
    #[test]
    fn no_secret_is_interpolated_into_a_log_or_format_macro() {
        // Names that carry a secret. A caller holding one of these holds something
        // that must not leave the process.
        //
        // THIS LIST WAS HALF THE CRATE'S CREDENTIALS, and the guard cannot be wider than what it
        // names. MEASURED: injecting `tracing::info!("the bot token is {bot_token}")` PASSED this
        // check, while the same injection with `server_key` FAILED it - so the mechanism works and
        // the coverage did not. Everything below `bot_token` is new.
        //
        // WHY IT WAS SHORT IN A WAY THAT LOOKS FINE: the five original names are the ones that appear
        // in LOGGING CONTEXT in the code that exists, so the list was written from the call sites
        // rather than from the credential inventory. A guard built that way catches today's leaks and
        // misses the next one - and the next one is what it is for.
        //
        // The additions come from the crate's credential HOLDERS: a value a caller must treat as a
        // secret whether or not it is logged today. `salt` is deliberately NOT here: `ip_tracking.rs`
        // has `panic!("poison the salt lock")`, which names the LOCK and never the salt's value, and a
        // guard that fires on correct code gets deleted.
        const SECRETS: &[&str] = &[
            "server_key",
            "api_key",
            "token_hash",
            "full_key",
            "presented_key",
            // A Telegram bot token authorises a wallet binding.
            "bot_token",
            // A raw single-use credential, before it is hashed or consumed.
            "raw_token",
            // The Midtrans signature the webhook verifies money against.
            "signature_key",
            // A password in the clear, on both the change and the reset paths.
            "password",
            "current_password",
            "new_password",
        ];
        const MACROS: &[&str] = &[
            "error!",
            "info!",
            "warn!",
            "debug!",
            "trace!",
            "println!",
            "print!",
            "dbg!",
            "panic!",
            "unreachable!",
        ];

        let mut findings: Vec<String> = Vec::new();
        let mut macros = 0usize;

        for path in source_files() {
            let raw = std::fs::read_to_string(&path).expect("every source file must read");
            let code = strip_comments(&raw);
            let lines: Vec<&str> = code.lines().collect();
            let name = path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();

            for (i, line) in lines.iter().enumerate() {
                if !MACROS.iter().any(|m| line.contains(m)) {
                    continue;
                }
                macros += 1;
                let end = (i + 6).min(lines.len());
                let window = lines[i..end].join(" ");
                for secret in SECRETS {
                    for (at, _) in window.match_indices(secret) {
                        let before = &window[..at];
                        if before.ends_with('%') || before.ends_with('{') {
                            findings.push(format!("{}:{} {secret}", name, i + 1));
                        }
                    }
                }
            }
        }

        // THE SCOPE FLOOR, which is the half of this guard that used to be missing.
        // A scan over a set it cannot report the size of asserts an absence over whatever
        // it happened to be handed: `source_files` covers the whole of `src` today, and
        // nothing here would notice a future skip that halved it. The floor is 25 against
        // the crate's own count - 38 at the time of writing, so the slack is 13 - and the
        // slack is the point: a floor AT the count is a tripwire that fires when a file is
        // deleted and says nothing about a scan that stopped early, which is the failure this
        // floor exists to catch. (This said "against 30 files", a count that was right when
        // written and is one of FOUR copies of the same stale figure in this file - the others
        // are the four `>= 18` floors, each of which carried the same sentence. Naming the
        // figure is what let it drift, so the number here is the one the property needs and the
        // sentence says which property that is.)
        let scanned = source_files().len();
        assert!(
            scanned >= 25,
            "the secret scan covered only {scanned} source files, so it is not looking at the whole crate"
        );

        // The vacuity guard. A scan that matched no macro would pass every assertion
        // above over an empty set, and would go on doing so if the macro names were
        // ever mistyped into a list that matched nothing.
        assert!(
            macros > 100,
            "only {macros} log or format macro calls were seen, far fewer than this crate has. A scan that finds almost nothing is not a scan."
        );
        assert!(
            findings.is_empty(),
            "a secret is substituted into a log or format macro:\n{findings:#?}\nA customer-facing privacy page states the Midtrans server key is never logged, and this is that claim in the only form that can be checked."
        );
    }
    /// Every document UNDER docs/ is TRIAGED: citation-checked, or listed with a reason.
    ///
    /// The scope is docs/ and the NAME SAYS SO, which is the point of the rename. There
    /// are 34 other markdown files here - a README per tool, the provider notes,
    /// AGENTS.md, and the agent scratch under .workbuddy-ai/ - and this test does not
    /// govern them. Most describe a CHECK rather than make a claim about the service: a
    /// tool README that could misstate a retention window is already covered by the check
    /// it describes, and the retention guard reads those files directly. Widening this one
    /// to all thirty-four would add a long list of entries that record nothing, and a list
    /// that records nothing is worse than a smaller one that is consulted.
    ///
    /// So the boundary is stated rather than implied. A name that outruns its parse is
    /// worse than a narrower name, because the name is what a reader trusts.
    ///
    /// WHY. The list of citation-checked documents above is hand-maintained, and a
    /// hand-maintained list fails SILENTLY in one direction: a new document arrives, is
    /// correct, and simply never joins the list. Nothing says so. This test makes the
    /// omission loud instead - the day a file appears in docs/, the suite goes red and
    /// someone decides whether it is a document an operator acts on.
    ///
    /// The decision it forces IS the judgement, because operational is not derivable: it
    /// is a judgement about whether a reader would be misled by a citation that points
    /// somewhere else. That is a call for a person, so the test does not make it - it
    /// makes it REQUIRED.
    ///
    /// Note what this does NOT claim: that the listed documents are the right ones, or
    /// that the others are correctly excluded. It claims only that every file has been
    /// considered, which is the part that was silently untrue.
    /// Every .md under a directory, as a path relative to docs/.
    fn collect_documents(dir: &std::path::Path, prefix: &str, out: &mut Vec<String>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            let path = entry.path();
            if path.is_dir() {
                collect_documents(&path, &format!("{prefix}{name}/"), out);
            } else if name.ends_with(".md") {
                out.push(format!("{prefix}{name}"));
            }
        }
    }

    /// A repository file, read from the workspace root rather than the crate root.
    fn read_repo_file(relative: &str) -> String {
        std::fs::read_to_string(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("..")
                .join(relative),
        )
        .unwrap_or_else(|e| {
            panic!("{relative} must be readable, or this test passes over nothing: {e}")
        })
    }

    /// Every `limits.<key>` a document NAMES exists in the shipped config.
    ///
    /// The instance this closes: a checklist told an operator to set
    /// `config/apikita.toml` `[limits]` for the rolling spend window, which is a code
    /// constant. An operator told to change a value finds nothing to change, and a
    /// checklist item reads as an instruction rather than a description.
    ///
    /// THE DENYLIST IS DELIBERATE. `limits.md` is a FILENAME, and a naive
    /// `limits.([a-z_]+)` match takes it as the key `md` and reports a knob that does
    /// not exist. Rather than widen the regex - which is how a check becomes noisy and
    /// gets ignored - the two-letter file stems are listed, because a configuration key
    /// is not called `md` or `rs`. That is the same boundary as the citation matcher that
    /// read clock times, and the same rule: only check a claim where the claim is
    /// unambiguous.
    #[test]
    fn every_limits_key_a_document_names_exists_in_the_config() {
        // Parsed, not string-matched. The first version required a four-space indent the
        // file does not use, so it reported ten keys as MISSING that were all present -
        // the same class as a check that cries wolf, in the other direction: a guard that
        // fails when the code is right is as useless as one that passes when it is wrong.
        let declared_keys: std::collections::HashSet<String> = std::fs::read_to_string(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("..")
                .join("config")
                .join("apikita.toml"),
        )
        .expect("config/apikita.toml must be readable, or this passes over nothing")
        .lines()
        .filter_map(|line| {
            let trimmed = line.trim();
            if trimmed.starts_with('#') {
                return None;
            }
            let (key, _) = trimmed.split_once('=')?;
            let key = key.trim();
            (!key.is_empty() && key.chars().all(|c| c.is_alphanumeric() || c == '_'))
                .then(|| key.to_string())
        })
        .collect();

        // NINE OF TWENTY-SEVEN, and one of the nine is `log`, a file type this repository
        // does not have. So the list was written from imagination rather than measured, and it
        // happens to be adequate rather than correct: the eighteen extensions it does not name
        // (conf, css, env, html, js, json, mjs, py, svg, txt, and the rest) are not currently
        // referenced after `limits.`, so nothing is being missed today.
        //
        // THE LIMIT IS STATED rather than the list widened, and the reason is the one this file
        // keeps hitting: a hand-kept exclusion list is a second copy of a fact, and it goes
        // stale silently. Deriving the set from the repository's actual extensions would remove
        // the list, at the cost of a walk on every run - which is the right trade only once a
        // false positive has actually appeared.
        //
        // THE TRIGGER: if this guard ever reports a key that is a file, the fix is to derive
        // the set, not to append another stem. Appending is how a list becomes today's
        // vocabulary rather than today's truth.
        const FILE_STEMS: &[&str] = &["md", "rs", "ts", "sh", "sql", "yml", "toml", "log", "astro"];

        // Every markdown a reader of the product would be shown, and every file the
        // website renders, so a key named in either place is covered.
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
        let mut sources: Vec<std::path::PathBuf> = Vec::new();
        let mut stack = vec![root.clone()];
        let mut walked = 0usize;
        while let Some(dir) = stack.pop() {
            let Ok(entries) = std::fs::read_dir(&dir) else {
                continue;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                let name = entry.file_name().to_string_lossy().into_owned();
                if path.is_dir() {
                    // Skip build output, dependencies and the agent scratch.
                    if !matches!(
                        name.as_str(),
                        "target" | "node_modules" | ".agents" | ".git"
                    ) {
                        stack.push(path);
                    }
                } else if name.ends_with(".md") {
                    walked += 1;
                    sources.push(path);
                }
            }
        }
        sources.sort();

        let mut offenders: Vec<String> = Vec::new();
        let mut checked = 0usize;
        for path in &sources {
            let text = std::fs::read_to_string(path).unwrap_or_default();
            for hit in text.match_indices("limits.") {
                let rest = &text[hit.0 + "limits.".len()..];
                let key: String = rest
                    .chars()
                    .take_while(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == '_')
                    .collect();
                if key.is_empty() || FILE_STEMS.contains(&key.as_str()) {
                    continue;
                }
                checked += 1;
                if !declared_keys.contains(key.as_str()) {
                    offenders.push(format!("{}: limits.{key}", path.display()));
                }
            }
        }

        // A FLOOR WITH SLACK, never AT the measured value: an exact floor is a tripwire that passes
        // today, fails the moment one file is deleted, and cannot tell a whole-repo walk from a
        // narrowed one that happens to reach the same number. The count is content; the scope is
        // what this floor guards.
        //
        // WHAT `walked` COUNTS, stated because this comment used to name a different measurement.
        // It is incremented on `name.ends_with(".md")` in the walk above - MARKDOWN FILES UNDER THE
        // REPO ROOT - and never on `.rs` files. The comment used to justify the number with
        // "server/src holds exactly 30 .rs files", which was true when written and is now 38, and
        // which measured a different tree in a different unit: nothing here ever counts Rust files.
        // A figure that is right about something else is worse than a stale one, because it reads as
        // a measurement of this floor. The `.md` count is 78 at the time of writing, so the floor of
        // 18 carries slack of 60 - which is the property being bought, not the figure.
        assert!(
            walked >= 18,
            "only {walked} source files were read, so this is not walking the crate"
        );
        assert!(
            checked >= 3,
            "only {checked} limits key(s) were checked, so this passes over the documents"
        );
        assert_eq!(
            offenders,
            Vec::<String>::new(),
            "a document names a config key that does not exist, so an operator told to set it finds nothing to set."
        );
    }

    /// Every config key is either READ by the code, or says so where it is set.
    ///
    /// Rounds of measuring turned up a dozen keys nothing reads, and the ones that
    /// mattered were not wrong but UNREADABLE AS CONTROLS: a low-balance warning
    /// described as being sent by a bot that cannot send it, a general
    /// wallet-mutation limit that was really a specific one, a context ceiling whose
    /// comment claimed it fed the pre-flight check that does not read it.
    ///
    /// The check is on the CONFIG, not the code, because the code cannot be wrong about
    /// a key it ignores. A key counts as read if it appears as a field ACCESS - a
    /// leading dot - which rules out the three things that make an unwired key look
    /// wired: a struct declaration, a struct literal in a fixture, and the parser key
    /// list. Still a heuristic, and described as one in docs/testing.md.
    ///
    /// Three escapes, each deliberate rather than convenient:
    ///
    /// - ZERO needs no marker. A key set to 0 whose comment says 0 disables it is
    ///   self-consistent: the feature is off, so nothing depends on it being read.
    /// - LABELS are unread on purpose. A description is documentation written in the
    ///   file the documentation belongs in, and six NOT ENFORCED markers saying so
    ///   would be a wall.
    /// - A comment block carries to the next BLANK line, not to the next assignment, and
    ///   is not consumed by the key it sits above. A TOML file documents a GROUP, and a
    ///   marker written once above a pair is the natural way to write it - the first
    ///   version of this test failed on the second key of an annotated pair, which is
    ///   the test being wrong about how people write TOML rather than the file.
    #[test]
    fn an_unread_config_key_says_so_where_it_is_set() {
        const MARKERS: &[&str] = &[
            "NOT ENFORCED",
            "RECORDED, NOT",
            "no mechanism",
            "to disable",
        ];
        const LABELS: &[&str] = &["description"];

        let config = read_repo_file("config/apikita.toml");

        // Every Rust file in the crate as one string, so a read is a substring test.
        let mut source = String::new();
        let mut stack = vec![std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src")];
        let mut files = 0usize;
        while let Some(dir) = stack.pop() {
            let Ok(entries) = std::fs::read_dir(&dir) else {
                continue;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    stack.push(path);
                } else if path.extension().is_some_and(|e| e == "rs") {
                    files += 1;
                    source.push_str(&std::fs::read_to_string(&path).unwrap_or_default());
                }
            }
        }

        let mut above = String::new();
        let mut checked = 0usize;
        let mut total = 0usize;
        for line in config.lines() {
            let trimmed = line.trim();
            if trimmed.starts_with('#') {
                above.push_str(trimmed);
                above.push(' ');
                continue;
            }
            if trimmed.is_empty() {
                above.clear();
                continue;
            }
            let Some((key, value)) = trimmed.split_once('=') else {
                continue;
            };
            let key = key.trim().to_string();
            let value = value.split('#').next().unwrap_or("").trim().to_string();
            total += 1;

            let read = source.contains(&format!(".{key}"))
                || LABELS.contains(&key.as_str())
                || !key.chars().all(|c| c.is_alphanumeric() || c == '_');
            if read || value == "0" {
                continue;
            }

            assert!(
                MARKERS.iter().any(|m| above.contains(m)),
                "config key `{key} = {value}` is read nowhere in the crate and its comment carries none of {MARKERS:?}. Wire it, or mark it where it is set - a key that is neither is a control the operator believes they have."
            );
            checked += 1;
        }

        assert!(
            // Slack, for the reason the other source walks carry: a floor equal to the file
            // count cannot distinguish a whole-crate walk from a narrowed one. This walk DOES
            // count `.rs` files under `server/src`, so the figure moves with the crate - it is
            // 38 at the time of writing, and a floor AT it would be a tripwire.
            files >= 18,
            "only {files} Rust files were read, so nothing can look read"
        );
        assert!(
            total >= 40,
            "only {total} assignments were parsed from the config file"
        );
        assert!(
            checked > 0,
            "no unread key was checked, so this passes over every key"
        );
    }

    /// Every retention WINDOW the documents promise is a window the nightly sweep
    /// deletes by, and the sweep deletes nothing the documents have not promised.
    ///
    /// This is the guard whose absence let a promise sit in a document for months
    /// with no code behind it. data-retention.md has said 7 days for
    /// link_redemption_attempts since before the sweep existed; nothing deleted a
    /// single row, and no test compared the two lists, because each was individually
    /// plausible. The two documents agreed with each other and neither agreed with the
    /// shell script.
    ///
    /// Both directions are asserted, because each has failed here. The forward one
    /// catches a promise with no enforcement; the reverse catches a delete nobody
    /// promised, which is a different kind of surprise - a table emptied on a schedule
    /// nobody agreed to. A blank count is not a zero count, and an unlisted table is not
    /// a harmless one.
    ///
    /// The windows live in the SWEEP and the promises in the DOCUMENTS, so the sweep is
    /// the definition and the map below is the transcription - the same arrangement as
    /// the reconciliation query, and for the same reason: a transcription is only safe if
    /// it is checked, which is what the two loops are.
    #[test]
    fn every_promised_retention_window_is_a_window_the_sweep_deletes() {
        const SWEEP: &[(&str, u32, i64)] = &[
            ("key_ip_seen", 7, crate::ip_tracking::SEEN_RETENTION_DAYS),
            ("key_ip_daily", 90, crate::ip_tracking::DAILY_RETENTION_DAYS),
            ("usage_daily", 730, crate::db::USAGE_DAILY_RETENTION_DAYS),
            ("usage_events", 90, crate::db::USAGE_EVENTS_RETENTION_DAYS),
            ("sessions", 30, crate::db::SESSION_RETENTION_DAYS),
            (
                "link_redemption_attempts",
                7,
                crate::db::LINK_ATTEMPT_RETENTION_DAYS,
            ),
            (
                "auth_attempts",
                7,
                crate::ip_tracking::AUTH_ATTEMPT_RETENTION_DAYS,
            ),
            // ADDED WITH THE GUARD THAT FOUND IT. `link_code_issues` was swept by
            // `ip_tracking::purge_expired` at seven days and deleted by NOTHING in
            // production: the maintenance entrypoint, which is what actually runs,
            // never mentioned the table, and neither did data-retention.md. So the
            // window existed in the code, had no promise, and had no enforcement -
            // and this list, which is the transcription of the sweep, was missing it
            // in exactly the way that made both gaps invisible.
            (
                "link_code_issues",
                7,
                crate::ip_tracking::LINK_CODE_ISSUE_RETENTION_DAYS,
            ),
            // THE THIRD TABLE TO ARRIVE WITH A PROMISE AND NO DELETE, and the one
            // that shows how a gap hides: `link_codes` was the exception this sweep's
            // own doc-comment used to name - "its own `+24h after use/expiry` rule is a
            // different shape" - while `docs/data-retention.md:77` and
            // `website/src/lib/privacy.ts` stated the window to customers. The
            // exception had been overtaken by `identity_tokens` below, which is the
            // same expires-then-delete shape, so it was a stale decision wearing a live
            // one's clothes. The ONLY delete was `issue_link_code` removing the one code
            // it superseded, so a code requested, never redeemed and never replaced had
            // no delete path at all.
            //
            // The window is ONE day of grace, and the number is not a plain period: a
            // code is terminal when it is redeemed OR when it expires unredeemed, and
            // the grace runs from whichever came first - so the predicate is
            // `COALESCE(used_at, expires_at)`, the shape `sessions` already uses for
            // revoked-vs-expired. The entrypoint passes the same 1, and
            // `docs/data-retention.md` states the window as "used or expired + 1 day".
            ("link_codes", 1, crate::db::LINK_CODE_LAG_GRACE_DAYS),
            // THE EXPIRED-LINK SWEEP, and the only row here whose window is NOT a
            // period. A verification or reset link is not kept for N days; it is
            // stale when it expires, and the sweep deletes on `expires_at <= now`.
            // Zero is therefore the honest number and not a placeholder - see
            // `db::IDENTITY_TOKEN_LAG_DAYS`, which is the same zero expressed for
            // the lag report.
            //
            // ADDED WITH THE SAME GUARD. `identity::tokens::purge_expired` had a
            // unit test and no caller of any kind, so this row's absence from the
            // list was one of three places the gap went unmentioned.
            ("identity_tokens", 0, crate::db::IDENTITY_TOKEN_LAG_DAYS),
        ];

        // Which document states each window. Not the same file throughout, which is
        // part of why this went unnoticed: the promise and the code were never in one
        // place to be compared.
        // TWO files, named here rather than globbed. Every markdown file under docs/
        // is read and triaged by the guard below, so a THIRD document gaining a
        // retention window is the case this misses - and it is a narrow one, because
        // the other two are the only documents that state one, and the triage list is
        // the thing that would change if that stopped being true.
        let sources = [
            read_repo_file("docs/data-retention.md"),
            read_repo_file("docs/ip-tracking.md"),
        ];
        let published = sources.join("\n");

        for (table, days, constant) in SWEEP {
            // THE SHELL LITERAL MUST BE THE RUST CONSTANT, for EVERY TABLE IN THE LIST,
            // which is the check that did not exist while these windows lived as literals
            // typed into the entrypoint. The privacy page, the policy table and the metrics
            // endpoint all read the numbers from Rust while the sweep read them from a
            // script, so nothing tied any promise to the thing that enforces it - a sweep
            // could be deleting at 30 days while every document said 7, and every other
            // check here would still pass because they compare the documents to the
            // CONSTANTS.
            //
            // It started as a special case for the one window that had no constant at all,
            // which is the shape this class of bug usually takes: fixed once, in one
            // place, and the other five left because they already had numbers to copy.
            assert_eq!(
                *days as i64,
                *constant,
                "the entrypoint deletes {table} after {days} days while the Rust constant is {constant}. One of the two is the promise and the other is the enforcement, and every other check in this file compares documents to the constant rather than to the script."
            );

            // The table must be named somewhere in the published documents, and the
            // window must be one of the two renderings a document uses: N days, or
            // N months where 730 stands for 24.
            assert!(
                published.contains(table),
                "the nightly sweep deletes `{table}` after {days} days, but neither data-retention.md nor ip-tracking.md mentions it. A table emptied on a schedule nobody agreed to is not harmless - say what it is and when."
            );
            let stated_days = format!("{days} days");
            let stated_months = if *days == 730 {
                "24 months".to_string()
            } else {
                String::new()
            };
            assert!(
                published.contains(&stated_days) || (!stated_months.is_empty() && published.contains(&stated_months)),
                "the sweep deletes `{table}` after {days} days, but no document states {days} days for it (nor 24 months for the 730 case). A window nobody was told about is not a promise."
            );
        }

        // THE REVERSE. Every retention_delete against a table that is not in the list
        // above is a delete this check has not agreed to.
        let entrypoint = read_repo_file(".docker/maintenance/entrypoint.sh");
        let mut swept: Vec<(String, i64)> = Vec::new();
        for line in entrypoint.lines() {
            if !line.contains("retention_delete") {
                continue;
            }
            let Some(rest) = line.split("$DB_FILE").nth(1) else {
                continue;
            };
            // Splitting on the closing paren is wrong here: the sessions call passes a
            // quoted COALESCE(...) that contains one, and the first paren is inside it.
            // The last token before the assignment ends is the day count, with the paren
            // trimmed off - which reads the same for `key_ip_seen 7)` and for
            // `... "COALESCE(revoked_at, expires_at)" 30)`.
            let tokens: Vec<&str> = rest.split_whitespace().collect();
            let Some(name) = tokens.iter().find(|t| !t.contains('"')).copied() else {
                continue;
            };
            // The DAY COUNT the script actually passes, not the one written in this test.
            // Reading the name only was the gap: the list above is a hand-kept copy, so
            // comparing it to a constant only proves the copy agrees with the constant
            // and says nothing about the script. A mutation changing the entrypoint from
            // 30 to 45 passed the previous version of this check.
            let days = tokens
                .iter()
                .rev()
                .find_map(|t| t.trim_end_matches(')').parse::<i64>().ok())
                .unwrap_or_default();
            swept.push((name.to_string(), days));
        }

        // The vacuity guard: a parse that found nothing would agree with anything.
        assert!(
            // SLACK, not the count. The entrypoint has exactly seven retention deletes, and
            // a floor of seven is a tripwire: it fails the day one is removed and cannot
            // tell a full parse from a partial one that still reaches seven. Five catches a
            // parse that lost a third of them, which is the failure the floor is for.
            swept.len() >= 5,
            "only {} retention deletes were parsed from the entrypoint",
            swept.len()
        );

        for (table, days) in &swept {
            let claimed = SWEEP
                .iter()
                .find(|(t, _, _)| t == table)
                .unwrap_or_else(|| {
                    panic!(
                        "the entrypoint deletes `{table}` but no window in this test claims to. Either the promise is missing from the documents, or this list is out of date - both are worth knowing, and neither may be resolved by silently editing the list."
                    )
                });
            assert_eq!(
                *days,
                claimed.2,
                "the entrypoint deletes `{table}` after {days} days while every document, the metrics endpoint and the Rust constant say {}. The script is the only place the number is not read from, and it is the only place that enforces it.",
                claimed.2
            );
        }
    }

    /// Every DURATION the customer page commits to also appears in the policy
    /// document it says it comes from.
    ///
    /// WHY THIS IS SEPARATE from the retention guard beside it, which is the whole
    /// point. That one reads data-retention.md and ip-tracking.md and checks the
    /// windows against the constants. It does not read the page a CUSTOMER reads.
    ///
    /// That omission was not theoretical: the 90-day per-request window was wrong on
    /// the customer page and right in both internal documents, and the two internal
    /// ones being right is exactly why nobody noticed. One assertion was added for
    /// that one number, and the other rows were left with the same exposure - which is
    /// how a one-off becomes an exception rather than a rule.
    ///
    /// WHY NUMBERS AND NOT ROWS. Matching row-to-row would need a mapping from ten
    /// page rows to the document sections they summarise, and that mapping is a
    /// judgement that would drift. What cannot drift is arithmetic: a duration
    /// printed for a customer has to be a duration the policy states. The rows that
    /// are not durations - Forever, Until deleted by user, Same as review, Keep
    /// record - are deliberately outside it, because they are not numbers and a
    /// check that tried to match them would be guessing.
    #[test]
    fn every_duration_the_customer_page_commits_to_is_in_the_policy_document() {
        let page = std::fs::read_to_string(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("..")
                .join("website")
                .join("src")
                .join("lib")
                .join("privacy.ts"),
        )
        .expect("website/src/lib/privacy.ts must be readable, or this passes over nothing");
        let policy = std::fs::read_to_string(doc_path("data-retention.md"))
            .expect("docs/data-retention.md must be readable, or this passes over nothing");

        // The retention table, as the page writes it: what / keep / why.
        let mut durations: Vec<String> = Vec::new();
        // The keeps that state no unit at all - the page's word-periods.
        let mut words: Vec<String> = Vec::new();
        // THE ROWS THE PARSER COULD NOT READ, tracked separately so a shape change is loud.
        //
        // WHY, and it is the same defect this file's price-card guard was fixed for in the round
        // before this comment was written. Every failure below is a `continue`, and a row that stops
        // matching the parser simply stops being compared - the vacuity floor cannot see it, because
        // the floor sits at 4 against 8 rows. MEASURED: changing ONE duration row's `, why:` key to
        // `, whyText:` took the parse from 8 rows to 7 and left this test GREEN. That row's period
        // then went unchecked, so the page could have changed it to a wrong number in the same edit
        // and nothing would have said so.
        //
        // SCOPE, because `privacy.ts` holds THREE `{ what: ... }` tables and only one of them is the
        // retention table. `stored` carries `where:` and `notStored` carries a bare `why:`; neither
        // has a `keep:` and neither is a period a customer is promised. A first version of this
        // assertion ran over the whole file and failed on a CORRECT tree with nine "unreadable" rows
        // that were simply not retention rows - so the scan starts at `export const retention` and
        // stops at the closing bracket.
        let mut unreadable: Vec<String> = Vec::new();
        let mut rows_seen = 0usize;
        let retention_table: Vec<&str> = {
            let mut out = Vec::new();
            let mut inside = false;
            for line in page.lines() {
                if line.trim_start().starts_with("export const retention") {
                    inside = true;
                    continue;
                }
                if inside && line.trim() == "];" {
                    break;
                }
                if inside {
                    out.push(line);
                }
            }
            out
        };
        for line in &retention_table {
            let Some(rest) = line.trim().strip_prefix("{ what:") else {
                continue;
            };
            rows_seen += 1;
            let Some((_, after_what)) = rest.split_once(", keep: '") else {
                unreadable.push(format!("no `, keep: '` in {rest:.40}"));
                continue;
            };
            let Some((keep, _)) = after_what.split_once("', why:") else {
                unreadable.push(format!("no `', why:` in {rest:.40}"));
                continue;
            };
            // A duration and nothing else. `30-90 days` is one, because a customer
            // reading a range is still being told a period.
            if keep.contains("day") || keep.contains("month") || keep.contains("hour") {
                durations.push(keep.to_string());
            } else {
                words.push(keep.to_string());
            }
        }

        // EVERY ROW IN THE RETENTION TABLE WAS UNDERSTOOD. A row listed here is one whose period - if
        // it has one - is no longer being compared against the policy document.
        assert!(
            unreadable.is_empty(),
            "{} row(s) in the `retention` table of website/src/lib/privacy.ts carry a shape this \
             parser cannot read, so their periods are not being checked at all: {unreadable:?}. The \
             rows are written `{{ what: '...', keep: '...', why: '...' }}`; a row that no longer \
             matches has left the check silently, and the floor below cannot see it.",
            unreadable.len()
        );

        // AND THE TABLE IS STILL THE SIZE THE FLOOR ASSUMES. Fifteen rows, eight of them durations,
        // so a table that lost a row entirely is visible even when every remaining row parses.
        assert!(
            rows_seen >= 15,
            "the `retention` table of website/src/lib/privacy.ts yielded only {rows_seen} row(s). \
             Fifteen rows were there when this floor was written; a lower count means a row was \
             DELETED, which every assertion below would accept because it only ever checks the rows \
             it can see."
        );
        // AND EVERY KEEP THAT STATES NO UNIT IS ONE OF THE PAGE'S WORD-PERIODS.
        //
        // THIS REPLACED A COUNT, and the count was the wrong instrument - MEASURED rather than
        // reasoned. The first version asserted `non_duration >= 5` against 7 word-periods, so
        // changing one row's `7 days` to `a week` left a floor with two of slack passing: the row
        // still parsed, still counted, and simply stopped being compared. Raising the floor to 7
        // would catch that one mutation and become a tripwire the other way - it would fail when a
        // row is ADDED that states a real duration, which is a change nobody should have to fight.
        //
        // A count cannot tell "a duration became prose" from "a row was added". The SHAPE can: a
        // keep either states a unit this test knows, or it is one of the words the page uses when
        // there is no number to give. Anything else is a period the comparison above silently
        // skipped, and it is named here.
        const WORD_PERIODS: &[&str] = &[
            "Forever",
            "Kept indefinitely; not deleted on request",
            "Same as the review",
            "Until used, or the link expires (24h for verification, 30m for a reset)",
            "Until used or expired + 24h",
            "Keep record, drop personal data",
        ];
        let unrecognised: Vec<&String> = words
            .iter()
            .filter(|w| !WORD_PERIODS.contains(&w.as_str()))
            .collect();
        assert!(
            unrecognised.is_empty(),
            "{} `keep:` value(s) in the retention table state no unit this test recognises and are \
             not one of the page's word-periods: {unrecognised:?}. Every other row's period is \
             compared against docs/data-retention.md; this one is compared against nothing, which is \
             how a duration rewritten as prose (a `7 days` becoming `a week`) leaves the check while \
             the run stays green. If the page gained a legitimate word-period, add it to \
             WORD_PERIODS in this test.",
            unrecognised.len()
        );

        // The vacuity guard: a parser that matched no row would agree with anything,
        // and this file is written in a style a small edit can change.
        assert!(
            durations.len() >= 4,
            "only {} duration row(s) were read from the page, so this test is not looking at the real retention table.",
            durations.len()
        );

        for keep in &durations {
            assert!(
                policy.contains(keep.as_str()),
                "the customer page promises {keep:?} and docs/data-retention.md does not state that period at all. The page says it restates the policy, so a number on it that the policy does not carry is a promise with no source."
            );
        }
    }
    /// No column in the schema is NAMED for a prompt or a completion.
    ///
    /// WHY THIS IS A SEPARATE TEST. The privacy page tells customers their prompts
    /// and completions are never stored, and the page's own comment says the claim is
    /// about the DATABASE and that nothing checks it: the log test covers the log, and
    /// a prompt column added to a request-path table for debugging would land, leave
    /// every test green, and the page would go on promising otherwise. Of every
    /// untested promise found in this repository this is the one a customer would
    /// care about most, and it was the only one nobody could check by reading.
    ///
    /// IT ALSO MATTERS THAT THE SCAN READS A COMPACT TABLE, and the first version of
    /// this test did not and the mutation proved it: a one-line create-table carrying a
    /// prompt column slipped straight through, because the line begins with CREATE and
    /// the parser was looking for a column at the start of a line. A table written that
    /// way is not hypothetical - it is how a quick one gets added - and a guard with
    /// that hole is the same silent kind of check this one exists to replace.
    ///
    /// WHY IT IS NAMED FOR NAMES. This looks for a COLUMN NAME, and a check that
    /// read the code instead would have to decide whether every string in the crate
    /// is prompt text, which is not a question a test can answer. The rule below is
    /// therefore deliberately narrow, and its limit is stated rather than hidden: a
    /// column called payload, raw or debug would NOT be caught. The second half of
    /// this control is the review prompt on the migration, and the honest description
    /// of the pair is that they cover the careless case and the deliberate one.
    ///
    /// Four of the forty-three TEXT columns are text a person wrote - reviews.body,
    /// review_sessions.body, admin_audit.detail, link_code_issues.reason - and none of
    /// them is named below, which is why this can be a name check at all. If a review
    /// column were ever renamed to one of these words the test would fail, and that
    /// failure would be worth reading rather than renaming the column.
    /// The word list below is a COPY of a claim the privacy page makes in prose, and a
    /// copy is a second source of truth. It drifted once already: the first version had
    /// three of the page's four words and had quietly swapped `response_body` for two of
    /// its own, so a column named `response_body` passed a test written for exactly that
    /// case. The list is aligned now, and THIS is what keeps it aligned — it reads the
    /// claim out of the page and fails if the two disagree, so the next edit to the
    /// comment cannot drift away without a red suite.
    ///
    /// It reads the sentence rather than a data structure because the page is written in
    /// prose, and a test that demanded a machine-readable list would be a test that
    /// dictated the shape of a customer-facing file. Where the two disagree this fails
    /// rather than choosing, because which one is right is a judgement about the claim
    /// and not about the code.
    #[test]
    fn the_guard_checks_exactly_the_words_the_privacy_page_claims_are_absent() {
        let page = std::fs::read_to_string(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("..")
                .join("website")
                .join("src")
                .join("lib")
                .join("privacy.ts"),
        )
        .expect("website/src/lib/privacy.ts must be readable, or this passes over nothing");

        let claimed: Vec<String> = page
            .lines()
            .find(|l| l.contains("no migration mentions"))
            .expect("the page must still state which names it checked for")
            .split("no migration mentions")
            .nth(1)
            .unwrap_or_default()
            .split(" -")
            .next()
            .unwrap_or_default()
            .split(|c: char| !(c.is_alphanumeric() || c == '_'))
            .filter(|w| w.len() > 2)
            .map(str::to_string)
            .collect();

        assert!(
            claimed.len() >= 3,
            "only {claimed:?} could be read from the page's claim; the sentence it lives in has probably been reworded and the sentence itself is what this test is about"
        );

        // The list in the guard, named here rather than reached into, so the two are
        // compared as values and a future edit to either is a one-line change.
        const GUARD: &[&str] = &[
            "prompt",
            "completion",
            "request_body",
            "response_body",
            "messages",
            "conversation",
        ];
        for word in &claimed {
            assert!(
                GUARD.contains(&word.as_str()),
                "the privacy page claims no migration mentions {word:?}, and the guard that enforces it does not look for that word. The page is right or the guard is wrong, and this test will not guess which - add the word, or correct the claim."
            );
        }
    }

    #[test]
    fn no_schema_column_is_named_for_a_prompt_or_a_completion() {
        // The words are taken from the page itself: its comment lists prompt,
        // completion, request_body and response_body as the names it checked for and did
        // not find. The first version of this test covered three of those four and
        // quietly swapped response_body for two of its own - so the guard and the claim
        // it was written to enforce were not saying the same thing, and a column named
        // response_body would have passed a test named for exactly that case. A guard
        // and its claim have to use the same list or one of them is decoration.
        const FORBIDDEN: &[&str] = &[
            "prompt",
            "completion",
            "request_body",
            "response_body",
            "messages",
            "conversation",
        ];

        // AND THE ADDRESS RULE, which is a pattern rather than a name.
        //
        // `docs/launch-checklist.md` Gate 4 says attempts are stored as a salted IP
        // hash, "NEVER the raw address" - the strongest privacy claim in the
        // documentation, and true today: every address-shaped column in the schema is
        // `ip_hash` or `distinct_ips`, a count. What was missing is that nothing kept it
        // true. The FORBIDDEN list above is a list of NAMES, and `ip` is not among them, so
        // a migration adding `ip_address TEXT` would pass every check in this file and turn
        // Gate 4's strongest claim false with a green suite.
        //
        // So this is not another word in a list. A column is an address if its name says
        // so, UNLESS the name also says it is a hash or a count - which is precisely the
        // distinction the schema already draws, and which a bare word list cannot express
        // without listing `ip_hash` and `distinct_ips` as exceptions forever.
        const ADDRESS_WORDS: &[&str] = &["ip", "addr", "address", "remote", "host"];
        // `distinct` is here because the schema's word for a COUNT of addresses is
        // `distinct_ips`, not `ip_count` - and the first run of this rule failed on it. A
        // guard written from a guess at a vocabulary finds the real vocabulary on its first
        // run, which is the `log` entry all over again: the list is not wrong, it is not
        // MEASURED, and the difference only shows when the rule is switched on.
        const ADDRESS_EXEMPT: &[&str] = &["hash", "count", "distinct", "prefix", "seen", "daily"];

        let migrations = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("migrations");
        let mut entries: Vec<_> = std::fs::read_dir(&migrations)
            .expect("server/migrations must be readable, or this passes over nothing")
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|e| e == "sql"))
            .collect();
        entries.sort();

        let mut columns: Vec<String> = Vec::new();
        for path in entries {
            let text = std::fs::read_to_string(&path).unwrap_or_default();
            for line in text.lines() {
                let trimmed = line.trim();
                if trimmed.starts_with("--") {
                    continue;
                }
                let Some((name, rest)) = trimmed.split_once(char::is_whitespace) else {
                    continue;
                };
                if !name.chars().all(|c| c.is_alphanumeric() || c == '_') {
                    continue;
                }
                // Only a column DECLARATION: an uppercase type, and a type this
                // schema actually uses.
                let upper = rest.trim_start().to_ascii_uppercase();
                if ["TEXT", "INTEGER", "REAL", "BLOB", "NUMERIC"]
                    .iter()
                    .any(|t| upper.starts_with(t))
                {
                    columns.push(name.to_string());
                }
            }
        }

        // The vacuity guard. A parser that matched nothing would pass over the whole
        // schema, and this is precisely a check where an empty set means nothing:
        // zero columns would trivially contain no prompt column.
        assert!(
            // RAISED from 60 to 90. The schema declares 119 columns, so a floor of 60 was
            // set from a measurement taken when the schema was smaller and drifted LOOSE as
            // it grew - it would pass on a scan that read half the columns, which is this
            // check missing a table entirely. The same rule as the tight floors, running
            // the other way: a floor not re-derived as the content moves stops being a
            // guard in whichever direction it was not watching.
            columns.len() >= 90,
            "only {} column(s) were read from the migrations, so this test is not looking at the real schema.",
            columns.len()
        );

        let offenders: Vec<&String> = columns
            .iter()
            .filter(|c| {
                let lower = c.to_ascii_lowercase();
                FORBIDDEN.iter().any(|f| lower.contains(f))
            })
            .collect();
        assert!(
            offenders.is_empty(),
            "schema column(s) {offenders:?} are named for prompt or completion text. The privacy \"
             page tells customers their prompts and completions are never stored. If a \"
             column like this is genuinely needed, change the page in the same change, \"
             and mean it - a disclosure nobody has checked is worth less than one that \"
             was deliberately written."
        );

        // Gate 4's "never the raw address", checked the same way and with the same care
        // about what the guard and the claim each say.
        let raw_addresses: Vec<&String> = columns
            .iter()
            .filter(|c| {
                let lower = c.to_ascii_lowercase();
                let names_an_address = ADDRESS_WORDS.iter().any(|w| lower.contains(w));
                let qualified = ADDRESS_EXEMPT.iter().any(|e| lower.contains(e));
                names_an_address && !qualified
            })
            .collect();
        assert!(
            raw_addresses.is_empty(),
            "schema column(s) {raw_addresses:?} look like a RAW network address. Gate 4 of the \
             launch checklist says attempts are stored as a salted IP hash and NEVER the raw \
             address, and the privacy page tells customers only a hash is kept. The schema draws \
             the line with names - ip_hash and distinct_ips qualify because the name says what it \
             is - so a column that does not qualify is holding the address itself. If a raw \
             address is genuinely needed, change the disclosure in the same change."
        );
    }
    /// A test named after a LOG must use a log capture, or say it is not one.
    ///
    /// THE SEVENTH RESTATEMENT, and the first one at TEST level rather than guard level.
    /// A test was called `recording_past_the_sharing_threshold_warns_once` and never
    /// observed a warning: it counted distinct IPs and stopped. The name promised a log
    /// assertion the body did not make, which is the same shape as a guard named after a
    /// document it never opened. The difference is that this one is mechanically checkable,
    /// because the crate already has `capture_logs` in webhooks.rs and two tests that use
    /// it - so a test can observe a log here, and one that does not is saying so by
    /// omission.
    ///
    /// The test that triggered this says `..._predicate_fires_at...` rather than
    /// `..._warning_fires...` for exactly this reason, and says in its doc comment that
    /// it stops one step short. That is the shape the rule asks for: if you cannot observe
    /// the thing, name the part you can.
    ///
    /// The exemption list is the set of names that CLAIM a log without being one - they
    /// are assertions about the `warn!` call site itself, and they are listed rather than
    /// silently passing, because a silent exemption and a forgotten test look identical.
    #[test]
    fn a_test_named_after_a_log_uses_a_capture_or_says_it_is_not_one() {
        // Not tests at all. `capture_logs` is the HELPER, and a guard that flagged the
        // thing it depends on would be a guard nobody could satisfy.
        const NOT_TESTS: &[&str] = &["capture_logs", "post_logged"];

        // Named for a log, and observes one through the crate's helper.
        const OBSERVES: &[&str] = &[
            "a_webhook_rejection_is_logged_under_the_documented_topup_rejected_event",
            "a_refund_refusal_is_logged_under_its_own_documented_event",
            "a_customer_prompt_never_reaches_the_log",
        ];
        // Named for a log, asserts about the CALL, and says so.
        const EXEMPTS: &[(&str, &str)] = &[
            (
                "internal_errors_log_their_status_field_when_a_subscriber_is_active",
                "asserts the field is passed to a subscriber, and says a subscriber is active",
            ),
            (
                "no_secret_is_interpolated_into_a_log_or_format_macro",
                "asserts an ABSENCE across the source, not an emission",
            ),
            (
                "summary_reports_an_alert_when_a_hold_exceeds_the_bound",
                "an alert row in a printed summary, not a tracing event",
            ),
        ];

        // EVERY .rs in the crate, walked rather than listed - and that is the second
        // restatement in one commit, caught by this guard's own mutation. The first
        // version named five files, which meant a test in ip_tracking.rs could claim a
        // log it never observes and the guard passed, which is the scope-too-narrow
        // failure this whole section is about. A hand-kept file list is a hand-kept copy
        // of the thing, and it was the fourth one this round.
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut sources: Vec<std::path::PathBuf> = Vec::new();
        let mut stack = vec![root];
        let mut walked = 0usize;
        while let Some(dir) = stack.pop() {
            let Ok(entries) = std::fs::read_dir(&dir) else {
                continue;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    stack.push(path);
                } else if path.extension().is_some_and(|e| e == "rs") {
                    walked += 1;
                    sources.push(path);
                }
            }
        }
        sources.sort();
        assert!(
            // Slack for the same reason as the other source walks: a floor equal to the file
            // count is a tripwire rather than a guard. This walk counts `.rs` files under
            // `server/src`, so the figure is 38 at the time of writing - the number was 30
            // when this comment was written, which is why the sentence names the PROPERTY
            // (slack below the count) rather than the count itself.
            walked >= 18,
            "only {walked} Rust files were walked, so this test is not looking at the whole crate"
        );

        // Names that look like a log claim, collected from the sources above.
        let mut claimed: Vec<(String, String)> = Vec::new();
        for path in sources {
            let Ok(text) = std::fs::read_to_string(&path) else {
                continue;
            };
            for line in text.lines() {
                // `fn ` ANYWHERE in the line, so the async and pub forms are covered:
                // most tests here are `async fn`, and a prefix match found one of six.
                let Some(at) = line.find("fn ") else {
                    continue;
                };
                let rest = &line[at + 3..];
                let name: String = rest
                    .chars()
                    .take_while(|c| c.is_alphanumeric() || *c == '_')
                    .collect();
                if name.is_empty() {
                    continue;
                };
                let lower = name.to_ascii_lowercase();
                if ["logged", "logs", "warns", "logging"]
                    .iter()
                    .any(|w| lower.contains(w))
                {
                    claimed.push((path.display().to_string(), name));
                }
            }
        }

        // The vacuity guard: a pattern change that matched nothing would pass over every
        // test in the crate.
        assert!(
            claimed.len() >= 3,
            "only {} test name(s) looked like a log claim, so this test is not looking at the real set.",
            claimed.len()
        );

        for (path, name) in &claimed {
            if NOT_TESTS.contains(&name.as_str())
                || OBSERVES.contains(&name.as_str())
                || EXEMPTS.iter().any(|(n, _)| n == name)
            {
                continue;
            }
            panic!(
                "{path} names a test `{name}` after a log, and it neither uses `capture_logs` nor lists itself as a call-site assertion. If it cannot observe the log, rename it after the part it does check - a predicate, a count, a field - and say in its doc comment that it stops short. A name that claims an observation the body does not make is the test-level form of a guard comparing a restatement."
            );
        }
    }

    /// Every method a doc-comment cites is a method this crate defines.
    ///
    /// `server/src/config.rs` cited a `from_config` constructor on `EmailSender` at
    /// four sites - on `smtp_host`, `smtp_port`, `smtp_password_env` and
    /// `request_timeout_seconds` - and no such method exists anywhere. The
    /// constructor is `new`. Three more citations in the same struct
    /// (`from_address`, `from_name`, `reply_to`) named `EmailSender::send`, which
    /// takes an already-built `Email` and reads no config at all.
    ///
    /// WHY A WRONG CITATION IS WORSE THAN NO CITATION. These comments exist to answer
    /// one question: where is this field consumed? A reader who follows a name that
    /// does not exist finds nothing, and the natural conclusion is that the field is
    /// UNUSED - which is the exact opposite of what the comment was placed there to
    /// say. Nothing compared the two, so seven citations sat wrong in a file whose
    /// whole purpose is to describe configuration accurately.
    ///
    /// WHAT THIS CANNOT SEE, stated plainly. It checks only the `::` form, so a prose
    /// mention ("the mailer's constructor") is out of scope. It checks that SOME item
    /// with that name is defined anywhere in the crate, not that it is reachable from
    /// the type named - `Parser::new` would pass if any `Parser` has a `new`, which is
    /// the common case and the honest limit of a text scan. It does not check that the
    /// cited method is the one that actually reads the field, which is the second half
    /// of the defect above and is not mechanically decidable.
    #[test]
    fn every_method_a_doc_comment_cites_is_a_method_this_crate_defines() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut sources: Vec<std::path::PathBuf> = Vec::new();
        let mut stack = vec![root.clone()];
        let mut walked = 0usize;
        while let Some(dir) = stack.pop() {
            let Ok(entries) = std::fs::read_dir(&dir) else {
                continue;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    stack.push(path);
                } else if path.extension().is_some_and(|e| e == "rs") {
                    walked += 1;
                    sources.push(path);
                }
            }
        }
        sources.sort();
        // Slack below the real count, for the same reason as the walks above: a floor
        // equal to the count is a tripwire that fires on a deletion and stays silent
        // when the walk stops early.
        assert!(
            walked >= 18,
            "only {walked} Rust files were walked, so this check is not looking at the whole crate"
        );

        // Every `fn NAME` in the crate, so a citation can be resolved. `fn ` anywhere
        // in the line covers the pub, async and unsafe forms.
        let mut defined: std::collections::HashSet<String> = std::collections::HashSet::new();
        // Every `pub NAME:` / `NAME:` struct field, because a doc comment that says
        // "read by `EmailSender::new`" and one that says "see the `smtp_host` field"
        // are the same kind of claim - a pointer to something the reader can go and
        // find. A field named as `T::f` is loose, but it resolves: the reader finds
        // `f` in `T`. Rejecting it would force thirteen correct comments to be
        // rewritten to satisfy a checker, which is how a check gets disabled.
        let mut fields: std::collections::HashSet<String> = std::collections::HashSet::new();
        let mut cited: Vec<(String, usize, String, String)> = Vec::new();

        for path in &sources {
            let Ok(text) = std::fs::read_to_string(path) else {
                continue;
            };
            let name = path
                .strip_prefix(&root)
                .unwrap_or(path)
                .display()
                .to_string();
            for (index, line) in text.lines().enumerate() {
                if let Some(at) = line.find("fn ") {
                    let rest = &line[at + 3..];
                    let ident: String = rest
                        .chars()
                        .take_while(|c| c.is_alphanumeric() || *c == '_')
                        .collect();
                    if !ident.is_empty() {
                        defined.insert(ident);
                    }
                }
                // A struct field: an identifier followed by `:` at the start of a
                // trimmed line, in the `pub NAME:` or bare `NAME:` form. Collected in
                // the same walk so the two sets cannot describe two versions of the
                // crate. Lowercase-initial and identifier-shaped, which excludes
                // labels, match arms and `let x: T` (those are never at line start
                // with the type after the colon alone on the line).
                let declaration = line
                    .trim_start()
                    .strip_prefix("pub ")
                    .unwrap_or(line.trim_start());
                if let Some((candidate, _)) = declaration.split_once(':') {
                    let candidate = candidate.trim();
                    let identifier_shaped = !candidate.is_empty()
                        && candidate.chars().all(|c| c.is_alphanumeric() || c == '_')
                        && candidate.starts_with(|c: char| c.is_ascii_lowercase() || c == '_');
                    if identifier_shaped {
                        fields.insert(candidate.to_string());
                    }
                }
                // Only DOC COMMENTS are scanned. A path written as `Type::method`
                // inside executable code is resolved by the compiler already, so
                // checking it here would be a second, weaker copy of the borrow checker.
                let trimmed = line.trim_start();
                let doc = trimmed.starts_with("///")
                    || trimmed.starts_with("//!")
                    || trimmed.starts_with("//");
                if !doc {
                    continue;
                }
                for (ty, method) in cites_a_method(line) {
                    cited.push((name.clone(), index + 1, ty, method));
                }
            }
        }

        // The vacuity guard. A pattern change that matched nothing would make every
        // assertion below pass over an empty set, which is how a citation check
        // silently stops checking anything.
        assert!(
            cited.len() >= 20,
            "only {} method citation(s) were found in doc comments, so this check is not \
             looking at the real set - and a citation check that finds nothing passes.",
            cited.len()
        );

        // WHICH TYPES ARE OURS. The rule can only be applied to a type this crate
        // defines: `Duration::from_secs` and `SqliteConnectOptions::from_str` name
        // other people's methods, and failing on them would make the check fire on
        // correct prose until somebody disabled it. So the crate's own type names
        // are collected first, and a citation of anything else is skipped.
        let ours = crate_type_names(&sources);

        let mut bad: Vec<String> = Vec::new();
        for (file, line, ty, method) in &cited {
            // A citation may name the item by a path (`identity::email::EmailSender::new`),
            // so the LAST segment is the type and the next is the method.
            if !ours.contains(ty) {
                continue;
            }
            if !defined.contains(method) && !fields.contains(method) {
                bad.push(format!(
                    "server/src/{file}:{line} cites `{ty}::{method}`, and `{ty}` is a type this \
                     crate defines with no `{method}` field and no `fn {method}`"
                ));
            }
        }
        assert!(
            bad.is_empty(),
            "these doc comments cite a method this crate does not define. A reader \
             following one finds nothing and concludes the field is unused, which is the \
             opposite of what the comment is for:\n{}",
            bad.join("\n")
        );
    }

    /// The name of every type this crate defines - `struct`, `enum`, `trait` and
    /// `type`, in either the `Name` or the `pub Name` form.
    ///
    /// This is what separates a citation this crate must honour from a citation of
    /// somebody else's method. `Duration::from_secs` is correct prose and must not
    /// fail; `EmailConfig::smtp_host` names a type this crate owns, so the reader
    /// can go and check it and the check does too.
    fn crate_type_names(sources: &[std::path::PathBuf]) -> std::collections::HashSet<String> {
        let mut names = std::collections::HashSet::new();
        for path in sources {
            let Ok(text) = std::fs::read_to_string(path) else {
                continue;
            };
            for line in text.lines() {
                let trimmed = line.trim_start();
                let rest = if let Some(rest) = trimmed.strip_prefix("pub(crate) ") {
                    Some(rest)
                } else if let Some(rest) = trimmed.strip_prefix("pub ") {
                    Some(rest)
                } else {
                    Some(trimmed)
                };
                let Some(rest) = rest else { continue };
                for keyword in ["struct ", "enum ", "trait ", "type "] {
                    let Some(after) = rest.strip_prefix(keyword) else {
                        continue;
                    };
                    let name: String = after
                        .chars()
                        .take_while(|c| c.is_alphanumeric() || *c == '_')
                        .collect();
                    if !name.is_empty() {
                        names.insert(name);
                    }
                }
            }
        }
        names
    }

    /// The type-member pairs a line cites, if any - `T`, `::`, `m` with no spaces.
    /// The shape is described rather than written because naming it literally would
    /// make this guard flag its own documentation.
    ///
    /// Deliberately narrow, because a loose pattern here produces FALSE FAILURES on
    /// ordinary prose and the fix for a noisy check is to disable it. It requires:
    ///
    ///   - an ACRONYM-CASE or CamelCase type immediately before `::` (`EmailSender`,
    ///     `AppState`) - not a lowercase path segment, which is a module;
    ///   - a LOWERCASE method after it, at least three characters, so `::new` and other
    ///     very short names do not drag in `std::fmt::Debug`-style noise from elsewhere;
    ///   - no `<`, `(`, `"` or a preceding `:::` on the pair, which excludes generic
    ///     bounds, call expressions and prose quoting a signature.
    fn cites_a_method(line: &str) -> Vec<(String, String)> {
        let mut found = Vec::new();
        let bytes: Vec<char> = line.chars().collect();
        let mut i = 0usize;
        while i < bytes.len() {
            if bytes[i] == ':' && i + 1 < bytes.len() && bytes[i + 1] == ':' {
                // Walk back over the TYPE.
                let mut start = i;
                while start > 0 && (bytes[start - 1].is_alphanumeric() || bytes[start - 1] == '_') {
                    start -= 1;
                }
                let ty: String = bytes[start..i].iter().collect();
                // Walk forward over the METHOD.
                let mut end = i + 2;
                while end < bytes.len() && (bytes[end].is_alphanumeric() || bytes[end] == '_') {
                    end += 1;
                }
                let method: String = bytes[i + 2..end].iter().collect();
                let looks_like_a_type =
                    ty.chars().next().is_some_and(|c| c.is_ascii_uppercase()) && !ty.is_empty();
                let looks_like_a_method = method.len() >= 4
                    && method.starts_with(|c: char| c.is_ascii_lowercase() || c == '_');
                // A preceding `::` does NOT disqualify the pair, and the first
                // version of this function got that exactly backwards: it rejected
                // any type preceded by `::`, which threw away the TYPE segment of
                // every qualified citation - `identity::email::EmailSender::new` is
                // the form this codebase actually writes, and it extracted NOTHING
                // from it. The guard then passed over the real set while reporting
                // success, which is the failure mode it exists to prevent. A
                // qualified path in front is the normal case, not a disqualifier;
                // the uppercase-initial test below is what separates a type from a
                // module segment.
                if looks_like_a_type && looks_like_a_method {
                    found.push((ty, method));
                }
                i = end.max(i + 2);
            } else {
                i += 1;
            }
        }
        found
    }

    /// Every model's margin is the ONE the decision record states.
    ///
    /// `docs/decisions.md` says, in a table: Margin value = `1.5 per model - no
    /// global default`, and Margin location = `Per model, never global`. Those two
    /// rows together are a decision to DE-CENTRALISE the margin and then to hold it
    /// uniform, and nothing enforced the second half. Each model carries its own
    /// `price`, so a model added at 1.6 would be a silent divergence from a recorded
    /// decision, on the one number that decides what a customer pays - and the
    /// customer-facing prices in website/src/lib/models.ts are derived from it.
    ///
    /// THE NUMBER IS READ FROM THE DECISION, not written here, for the same reason the
    /// checklist bindings are: a copy beside the constant is a second place to
    /// update, and it goes stale the way the claim would without it. If the decision
    /// moves to 1.6, this test follows it and then requires every model to match -
    /// which is the only way the decision can actually be changed without editing
    /// seven places.
    #[test]
    fn every_model_carries_the_margin_the_decision_record_states() {
        let decisions = read_repo_file("docs/decisions.md");
        let row = decisions
            .lines()
            .find(|l| l.contains("Margin value"))
            .expect("docs/decisions.md must still record a Margin value row");
        // The FIRST number in that row, which is the operative multiplier.
        let decided: f64 = row
            .split(|c: char| !(c.is_ascii_digit() || c == '.'))
            .find(|part| !part.is_empty())
            .unwrap_or_else(|| panic!("the Margin value row carries no number: {row}"))
            .parse()
            .expect("a decimal parses");
        // NO ASSERTION PINS THE VALUE, and that is a correction to the first version of
        // this test, which asserted `decided == 1.5`. That was a hand-kept copy of the
        // decision sitting inside the guard whose whole argument is that a copy goes
        // stale - and worse, it forbade a legitimate outcome: the margin is set by
        // DECISION, so a decision to move to 1.6 is not drift and should not fail here.
        //
        // What the decision changing does break is downstream, and that is not this
        // test's job to hold: the config comparison below still passes, because every
        // model would be moved with it, and what would NOT follow is the transcription
        // on the website. That risk is written down where the transcription is, with the
        // multiplication shown, because a reader repricing needs the arithmetic and not
        // a failure message from a file that has no way to know the price changed.
        let config = read_repo_file("config/apikita.toml");
        let mut model = String::new();
        let mut checked = 0usize;
        for line in config.lines() {
            let trimmed = line.trim();
            if let Some(rest) = trimmed.strip_prefix("name = ") {
                model = rest.trim_matches('"').to_string();
            } else if let Some(rest) = trimmed.strip_prefix("price = ") {
                let declared: f64 = rest
                    .split('#')
                    .next()
                    .unwrap_or("")
                    .trim()
                    .parse()
                    .unwrap_or_else(|_| panic!("model {model} has an unparseable price: {rest}"));
                assert_eq!(
                    declared, decided,
                    "model {model} declares price {declared} while the decision record states {decided}. The margin is per model BY DESIGN, so nothing else would notice a divergence - and this is the number the customer-facing prices are derived from."
                );
                checked += 1;
            }
        }

        // The vacuity guard: a parse that matched no model would agree with anything.
        assert!(
            // Six models, so the floor is four. A floor AT the count is a tripwire.
            checked >= 4,
            "only {checked} model price(s) were read from the config, so this is not looking at them all"
        );
    }
    /// Each bound claim carries the SAME NUMBER as the code, and this keeps no copy
    /// of either.
    ///
    /// The number is read OUT OF THE SENTENCE and compared to the constant. Listing
    /// the expected figure beside the constant would be a second place to update, and
    /// would go stale exactly the way these claims would without it - the defect this
    /// file has now found four times. So the only thing written down is the phrase to
    /// find; the value comes from the document and from Rust.
    ///
    /// THE DOCUMENT IS PART OF THE BINDING, which is the part the first attempt got
    /// wrong. Searching one document for every phrase found `20 distinct` nowhere in
    /// the launch checklist - it is in ip-tracking.md, and the real sentence is
    /// `>20 domestic-distinct`. A binding to the wrong file is a check that could
    /// have passed on the wrong text, which is worse than one that fails.
    ///
    /// Three claims, each verified to exist in the file named before this was
    /// written. A full check would bind every numeral in the documentation; the honest
    /// limit is that it binds these three and not the rest, stated rather than left to
    /// look like coverage.
    #[test]
    fn a_bound_claim_carries_the_number_the_code_has() {
        let bindings: [(&str, &str, i64); 3] = [
            (
                "docs/launch-checklist.md",
                "30 days absolute",
                crate::db::SESSION_RETENTION_DAYS,
            ),
            (
                "docs/ip-tracking.md",
                "SHARING_SUSPICION_IPS = 20",
                crate::ip_tracking::SHARING_SUSPICION_IPS as i64,
            ),
            (
                "docs/website/06-api-keys-and-limits.md",
                "Rolling window **30 days**",
                crate::routes::keys::SPEND_WINDOW_DAYS,
            ),
        ];

        let mut cache: std::collections::HashMap<&str, String> = std::collections::HashMap::new();
        let mut checked = 0usize;
        for (doc_path, phrase, constant) in bindings {
            let doc = cache
                .entry(doc_path)
                .or_insert_with(|| read_repo_file(doc_path));
            let line = doc
                .lines()
                .find(|l| l.contains(phrase))
                .unwrap_or_else(|| panic!("{doc_path} no longer contains the phrase {phrase:?}"));
            // The first run of digits in the sentence that carries the claim.
            let stated: i64 = line
                .split(|c: char| !c.is_ascii_digit())
                .find(|part| !part.is_empty())
                .unwrap_or_else(|| panic!("the phrase {phrase:?} carries no number: {line}"))
                .parse()
                .expect("a run of digits parses");
            assert_eq!(
                stated,
                constant,
                "{doc_path} says {stated} for {phrase:?} and the code says {constant}. The checklist and the key-limits page are what a customer and a launcher read."
            );
            checked += 1;
        }
        assert_eq!(checked, bindings.len(), "a binding was skipped");
    }
    /// Every RELATIVE markdown link in the documentation resolves to a file.
    ///
    /// The instance: the whitepaper told a reader to see
    /// `website/tests/retired-docs.test.ts` to understand why its figures must not come
    /// back. No such file has ever existed - the test is `retired-whitepaper.test.ts` -
    /// and the LINK TEXT said the same wrong name, so the sentence agreed with itself
    /// and disagreed with the filesystem. That shape survives review, because reading
    /// the sentence is exactly what a reviewer does.
    ///
    /// This is existence, not meaning: a link can resolve and still point at the wrong
    /// thing, which is the limit stated rather than hidden. What it does catch is a
    /// path that has MOVED, which is the common case and the one a reader pays for.
    ///
    /// The sweep that found the instance covered 48 files and 440 links, so the
    /// vacuity guards below are set from a real measurement rather than a guess.
    #[test]
    fn every_relative_markdown_link_resolves() {
        let repo = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("..");

        // THE WHOLE REPOSITORY, not docs/ and the two root READMEs. The first version
        // walked `docs/` only, and the name said "the documentation" - which is 30 files
        // wider than what it checked: every tool README, the config provider notes, the
        // nginx and maintenance READMEs, and the agent memory files. A guard narrower than
        // its own name is the error this session has just spent a round on, in the
        // direction that does not announce itself: a too-broad matcher cries wolf, and a
        // too-narrow one simply does not look.
        //
        // Those thirty were measured before this was widened: 77 relative links, none
        // broken. So the class was clean and the SCOPE was the defect, which is the
        // better of the two findings and the cheaper to fix.
        const SKIP: &[&str] = &[
            "node_modules",
            "target",
            ".git",
            "dist",
            "build",
            ".agents",
            ".workbuddy-ai",
        ];
        let mut sources: Vec<std::path::PathBuf> = Vec::new();
        let mut stack = vec![repo.clone()];
        while let Some(dir) = stack.pop() {
            let Ok(entries) = std::fs::read_dir(&dir) else {
                continue;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                let name = entry.file_name().to_string_lossy().into_owned();
                if path.is_dir() {
                    if !SKIP.contains(&name.as_str()) {
                        stack.push(path);
                    }
                } else if path.extension().is_some_and(|e| e == "md") {
                    sources.push(path);
                }
            }
        }
        sources.sort();

        let mut broken: Vec<String> = Vec::new();
        let mut checked = 0usize;
        for path in &sources {
            let text = std::fs::read_to_string(path).unwrap_or_default();
            let dir = path.parent().unwrap_or(path.as_path());
            for hit in text.match_indices("](") {
                let rest = &text[hit.0 + 2..];
                let Some(end) = rest.find(')') else { continue };
                let raw = &rest[..end];
                // An anchor is part of the target, not a separate link; and only
                // RELATIVE paths are ours to resolve.
                let target = raw.split('#').next().unwrap_or_default().trim();
                if target.is_empty()
                    || target.contains("://")
                    || target.starts_with("mailto:")
                    || target.starts_with('#')
                {
                    continue;
                }
                checked += 1;
                if !dir.join(target).exists() {
                    broken.push(format!("{} -> {target}", path.display()));
                }
            }
        }

        assert!(
            sources.len() >= 70,
            "only {} markdown files were read, so this is not walking the whole repository",
            sources.len()
        );
        assert!(
            checked >= 300,
            "only {checked} relative link(s) were checked, so this passes over the documentation"
        );
        assert_eq!(
            broken,
            Vec::<String>::new(),
            "a relative markdown link points at a file that does not exist, so a reader following it is told the path moved or never existed, with no way to tell which."
        );
    }

    /// The pricing document restates the shipped card, and is bound to the config.
    ///
    /// `docs/business/02-pricing.md` carries a table with the IDR off-peak and peak figures
    /// and, in the same row, the CNY source each was converted from. Two things are
    /// asserted, and the second is the one that matters:
    ///
    ///   1. each IDR figure is the CNY beside it times the stated FX - the document is
    ///      internally consistent;
    ///   2. each figure is a rate the FLASH model actually ships - the document restates
    ///      the config rather than a self-consistent set of its own.
    ///
    /// The second is the whole point, and it is the check a copy-against-copy comparison
    /// cannot give. There are four copies of this card - the config, the website, this
    /// document, and the margin decision - and when `deepseek-v4-pro input_peak` read
    /// 12045.50 where 4.50 x 2676.78 is 12045.51, THREE OF THE FOUR AGREED WITH EACH
    /// OTHER. Only the config's own arithmetic was wrong, so every check that compared a
    /// copy to a copy passed while a customer-visible price was a cent off its source.
    ///
    /// Asserting against the config's rates rather than only against the FX is what makes
    /// this different from the guard on `config/apikita.toml`, which asks whether each
    /// rate IS its own conversion. Together the two close the loop from both ends.
    #[test]
    fn the_pricing_document_restates_the_shipped_card() {
        let config = read_repo_file("config/apikita.toml");
        let doc = read_repo_file("docs/business/02-pricing.md");

        // The FX comes from the config header, never from this file: a second place to
        // update a number is the defect this test exists to find.
        let fx: f64 = config
            .lines()
            .find_map(|line| {
                let rest = line.trim().trim_start_matches('#').trim();
                let rest = rest.strip_prefix("1 CNY = ")?;
                let digits: String = rest
                    .split_whitespace()
                    .next()?
                    .chars()
                    .filter(|c| c.is_ascii_digit() || *c == '.')
                    .collect();
                digits.parse().ok()
            })
            .expect("the config header must state the CNY to IDR rate");

        // The flash model's six shipped rates, as numbers.
        let flash = config
            .split("[[models]]")
            .find(|block| block.contains("name = \"flash\""))
            .expect("the config must declare a model named flash");
        let mut shipped: Vec<f64> = Vec::new();
        for key in [
            "input_offpeak",
            "input_peak",
            "cache_read_offpeak",
            "cache_read_peak",
            "output_offpeak",
            "output_peak",
        ] {
            let line = flash
                .lines()
                .find(|l| l.trim_start().starts_with(key))
                .unwrap_or_else(|| panic!("the flash model no longer declares {key}"));
            let value = line
                .split('=')
                .nth(1)
                .unwrap_or_default()
                .split('#')
                .next()
                .unwrap_or_default()
                .trim();
            let value: f64 = value
                .parse()
                .unwrap_or_else(|_| panic!("{key} is not a number: {value:?}"));
            shipped.push(value);
        }
        assert_eq!(shipped.len(), 6, "the flash card must contribute six rates");

        // THE DOCUMENT'S OWN STATED FX, and this is the binding that closes the loop.
        // `docs/business/02-pricing.md` says "Converted at 1 CNY = 2,676.78 IDR" and
        // gives the rule. The FX is read from the CONFIG above, so every figure below is
        // checked against the config's rate - and if the document quoted a DIFFERENT rate,
        // every figure in it would convert correctly and the guard would say nothing: the
        // document would be consistent with the config's arithmetic while telling a reader
        // a different conversion. That is the same class as a self-consistent copy, one
        // level up, and it is invisible to any check that takes the rate from one file and
        // applies it to figures in another.
        let doc_fx: f64 = doc
            .lines()
            .find_map(|line| {
                let rest = line.split("1 CNY =").nth(1)?.trim();
                let digits: String = rest
                    .split_whitespace()
                    .next()?
                    .chars()
                    .filter(|c| c.is_ascii_digit() || *c == '.')
                    .collect();
                digits.parse().ok()
            })
            .expect("the pricing document must state the CNY to IDR rate it converted at");
        assert!(
            (doc_fx - fx).abs() < 0.001,
            "the pricing document says the card was converted at 1 CNY = {doc_fx} IDR while config/apikita.toml says {fx}. Every figure in the document converts correctly by the rate it quotes, so nothing else here would notice."
        );

        // A table cell that is a bare figure, possibly with a thousands separator.
        let figure = |cell: &str| -> Option<f64> {
            let t = cell.trim();
            if t.is_empty()
                || !t
                    .chars()
                    .all(|c| c.is_ascii_digit() || c == ',' || c == '.')
            {
                return None;
            }
            t.replace(',', "").parse().ok()
        };

        let mut rows = 0usize;
        for line in doc.lines() {
            if !line.starts_with('|') {
                continue;
            }
            let cells: Vec<&str> = line.split('|').collect();

            // The CNY source cell: two figures either side of a slash. The sign is not
            // matched, only the digits after it, because a sign is a shape and this is a
            // value.
            let cny_cell = cells.iter().find(|c| {
                c.contains('/')
                    && c.matches(|ch: char| ch.is_ascii_digit() || ch == '.')
                        .count()
                        >= 4
            });
            let Some(cny_cell) = cny_cell else { continue };
            let cny: Vec<f64> = cny_cell
                .split('/')
                .map(|part| {
                    let digits: String = part
                        .chars()
                        .filter(|ch| ch.is_ascii_digit() || *ch == '.')
                        .collect();
                    digits.parse().ok()
                })
                .collect::<Option<Vec<f64>>>()
                .unwrap_or_default();
            if cny.len() != 2 {
                continue;
            }

            let shown: Vec<f64> = cells.iter().filter_map(|c| figure(c)).collect();
            if shown.len() != 2 {
                continue;
            }
            rows += 1;

            for (i, label) in ["off-peak", "peak"].iter().enumerate() {
                let expected = (cny[i] * fx * 100.0).round() / 100.0;
                assert!(
                    (expected - shown[i]).abs() < 0.001,
                    "the pricing document quotes {} as {} for the {label} figure, but the CNY beside it is {} and {} x {fx} rounds to {expected}",
                    cells.iter().find(|c| !c.trim().is_empty()).unwrap_or(&"").trim(),
                    shown[i],
                    cny[i],
                    cny[i]
                );
                assert!(
                    shipped.contains(&shown[i]),
                    "the pricing document quotes {shown_i} but no flash model ships that rate - the document has restated a card of its own instead of the config's",
                    shown_i = shown[i]
                );
            }
        }

        // The vacuity guard, with slack: three rows today, and a table that loses its
        // figures would otherwise pass over an empty set.
        assert!(
            rows >= 3,
            "only {rows} pricing row(s) carried both figures and a CNY source, so this is not reading the table"
        );
    }
    /// Every rate in the price card is its stated CNY source times the stated FX.
    ///
    /// The config opens with the conversion (1 CNY = 2,676.78 IDR) and the rule for
    /// re-deriving: `IDR_rate = CNY_rate * 2676.78`. Every rate then carries its CNY
    /// origin in a trailing comment after a yen sign. This asks whether each rate IS that
    /// conversion.
    ///
    /// The instance: `deepseek-v4-pro` `input_peak` read 12045.50 where 4.50 x 2676.78 is
    /// 12045.51 - one cell of thirty-six, and the only inexact one. It survived because it
    /// is invisible: 12045.50 x 1.5 and 12045.51 x 1.5 both round to the 18,068 the
    /// customer is quoted, so nothing downstream disagreed. Only asking whether the number
    /// is the conversion of the number beside it found it.
    ///
    /// **FOUR VERSIONS OF THIS FAILED FIRST, all matchers of mine, and the reasons are
    /// recorded because each is a way a check is silently wrong:**
    ///
    ///   1. the header is a COMMENT and writes the rate with a comma, so a bare `1 CNY = `
    ///      and a bare float matched NOTHING and this failed on a wholly correct config;
    ///   2. the CNY is written after a YEN SIGN, not as the text `CNY `, so nothing
    ///      matched - caught by the vacuity floor below, not by reading the file;
    ///   3. the first version that matched reached past the rates and read the CNY out of
    ///      `price = 1.5  # 50% markup`, calling a MULTIPLIER a rate and reporting drift;
    ///   4. `split_once('=')` splits on the FIRST `=`, so the value still carried its comment
    ///      and would not parse - also caught by the vacuity floor.
    ///
    /// Every one of those would have been a green run. The floors are what made them loud,
    /// which is the whole argument for a floor on a check that reads a shape.
    #[test]
    fn every_rate_in_the_price_card_is_its_cny_source_times_the_stated_fx() {
        let config = read_repo_file("config/apikita.toml");

        // Read from the header, never written here, for the reason every floor in this
        // file carries: a copy beside the comparison goes stale and then asserts its own
        // staleness. The line is a comment and uses a thousands separator.
        let fx: f64 = config
            .lines()
            .find_map(|line| {
                let rest = line.trim().trim_start_matches('#').trim();
                let rest = rest.strip_prefix("1 CNY = ")?;
                let digits: String = rest
                    .split_whitespace()
                    .next()?
                    .chars()
                    .filter(|c| c.is_ascii_digit() || *c == '.')
                    .collect();
                digits.parse().ok()
            })
            .expect("the config header must state the CNY to IDR rate");
        assert!(
            fx > 0.0,
            "the stated FX rate is {fx}, which no price derives from"
        );

        // THE SIX RATE KEYS, NAMED, because a guard that reaches past its subject finds
        // something: an earlier version read the `price` multiplier's comment as a rate.
        // The trade is stated - a NEW rate class is unchecked until added here, and the
        // floor below fails if these six stop being found, so a seventh is visible as a
        // config key with no counterpart, the cheapest kind of miss to catch in review.
        const RATE_KEYS: &[&str] = &[
            "input_peak",
            "input_offpeak",
            "cache_read_peak",
            "cache_read_offpeak",
            "output_peak",
            "output_offpeak",
        ];

        let mut checked = 0usize;
        // EVERY RATE LINE THE CONFIG STATES, counted separately from what the check manages to read.
        //
        // WHY THE TWO COUNTS, and it is a MEASURED gap rather than a tidy-up. The loop below used to
        // `continue` on an unparseable CNY figure or value WITHOUT recording that it had skipped the
        // line, so a rate whose comment lost its `¥` figure simply stopped being checked - the exact
        // drift this guard exists to catch. The vacuity floor was the only thing that could notice,
        // and it sits at 30 against 36 lines, so MEASURED: stripping the CNY figure from SIX rate
        // lines left all 31 doc_claims guards GREEN, and the seventh turned the run red. Six prices
        // could have lost their source unnoticed.
        //
        // The floor cannot be raised to close this - it would then be a tripwire at the count, which
        // the sibling floors in this file argue against. Counting the SKIPS is what makes each one
        // visible: `existing` is read from the config, `checked` from the guard's own success, and
        // the difference is asserted to be zero.
        let mut existing = 0usize;
        let mut skipped: Vec<String> = Vec::new();
        for line in config.lines() {
            let trimmed = line.trim();
            // The COMMENT IS SPLIT OFF FIRST, or the value carries it and will not parse.
            let Some((before_comment, comment)) = trimmed.split_once('#') else {
                continue;
            };
            let Some((key, value)) = before_comment.split_once('=') else {
                continue;
            };
            if !RATE_KEYS.contains(&key.trim()) {
                continue;
            }
            existing += 1;

            let cny: String = comment
                .chars()
                .filter(|c| c.is_ascii_digit() || *c == '.')
                .collect();
            let (Ok(cny), Ok(stated)) = (cny.parse::<f64>(), value.trim().parse::<f64>()) else {
                skipped.push(format!("{} = {}", key.trim(), value.trim()));
                continue;
            };
            checked += 1;

            // The config states every rate ROUNDED TO TWO DECIMALS, so the comparison is
            // against the rounded product. Comparing raw with a tolerance sitting on half
            // a cent flagged 2.25 x 2676.78 = 6022.755 stated as 6022.76 - correct rounding,
            // not drift.
            let expected = (cny * fx * 100.0).round() / 100.0;
            assert!(
                (expected - stated).abs() < 0.001,
                "{key} is stated as {stated} but its comment says CNY {cny}, and {cny} x {fx} rounds to {expected}. A rate and the CNY it came from have drifted apart."
            );
        }

        // EVERY RATE LINE WAS READ, which the floor below cannot say on its own. A line whose CNY
        // figure or value will not parse is reported here rather than passed over in silence.
        assert!(
            skipped.is_empty(),
            "{} rate line(s) carry a value or a CNY comment this check cannot read, so they are \
             NOT being compared against the price card: {skipped:?}. A rate that quietly leaves \
             this check is the drift it exists to catch - either the line lost its `¥` figure, or \
             its comment no longer states one. MEASURED: six such lines could drop out unnoticed \
             before this assertion existed, because the floor below sits at 30 against 36 lines.",
            skipped.len()
        );

        // The vacuity guard, and it is not decorative: it caught two of the four versions
        // above, both of which would otherwise have been green runs.
        assert!(
            checked >= 30,
            "only {checked} rate(s) with a CNY source were found, so this is not checking the price card"
        );

        // AND THE TWO COUNTS AGREE, so the assertion above cannot be satisfied by a config that
        // stopped stating rates at all: with `existing` at zero, the `skipped` list would be empty
        // too and the run would be green.
        assert_eq!(
            checked, existing,
            "{checked} rate(s) were compared but {existing} exist in the config. The difference is \
             what the assertion above should have caught; if it did not, this count is the one to \
             trust."
        );
    }
    /// Every table the schema creates is either DISCLOSED on the privacy page or
    /// recorded here as deliberately unused.
    ///
    /// WHY THIS DIRECTION, because the opposite one is already guarded. A row that
    /// contradicts the code is a false statement somebody can catch by reading; a
    /// table MISSING from the disclosure is invisible, and it is the one a customer
    /// would be angry about. Three were missing until two rounds ago: the key_ip
    /// tables, the link-code attempts, and the audit rows.
    ///
    /// The pairing matters. Each DISCLOSED entry names a phrase that must actually
    /// appear in the page, so the map cannot drift from the text it describes; and
    /// every table must appear on one side or the other, so a new table cannot be
    /// added and left unmentioned. Adding one to a migration makes this red until
    /// somebody decides whether a customer is told about it.
    #[test]
    fn every_table_is_either_disclosed_to_the_customer_or_recorded_as_unused() {
        // table -> a phrase that must be present in website/src/lib/privacy.ts.
        const DISCLOSED: &[(&str, &str)] = &[
            ("accounts", "Account record (id, status, sign-up time)"),
            ("wallets", "Wallet balance + ledger"),
            ("ledger", "Wallet balance + ledger"),
            ("topups", "Top-up history"),
            ("usage_daily", "Token usage per day"),
            ("usage_events", "Per-request usage"),
            ("api_keys", "API keys"),
            ("reviews", "Reviews + edit history"),
            ("review_history", "Reviews + edit history"),
            ("review_sessions", "Reviews + edit history"),
            ("telegram_links", "Telegram ID"),
            ("sessions", "Sessions"),
            ("key_ip_seen", "Salted IP hash of API-key traffic"),
            ("key_ip_daily", "Salted IP hash of API-key traffic"),
            (
                "link_redemption_attempts",
                "Salted IP hash of failed link-code attempts",
            ),
            ("auth_attempts", "Sign-in attempt counters"),
            ("link_codes", "Telegram link codes"),
            ("link_code_issues", "Telegram link codes"),
            ("admin_audit", "operator audit rows"),
            // The identity port moved email, the password hash and the Google link
            // OUT of PocketBase and INTO `identities`; `privacy.ts` was updated in the
            // same change. Every row here is a phrase that must appear on the page.
            ("identities", "Email verification and password-reset links"),
            (
                "identity_tokens",
                "Email verification and password-reset links",
            ),
        ];

        // Tables that exist and hold nothing. A reason is required, because an empty
        // string is not a reason and a future reader cannot tell deliberate from
        // forgotten.
        const UNUSED: &[(&str, &str)] = &[
            (
                "wallets_pending_rebuild",
                "a placeholder held open for a future wallet migration; no migration creates it, and it is listed only so this map has an entry that is not a live table - if you are reading this looking for the real UNUSED set, it is now empty, because the identity port gave the last one a writer",
            ),
        ];

        let privacy = read_repo_file("website/src/lib/privacy.ts");

        // EVERY migration, not the first one. This test is called
        // every_table_is_either_disclosed_to_the_customer_or_recorded_as_unused, and it
        // read exactly one of four files: link_redemption_attempts and link_code_issues
        // are created by LATER migrations and were invisible to it. They passed because
        // they had been listed by hand, which is luck, not construction - a fifth
        // migration adding a table would have gone entirely unseen, in a test whose whole
        // purpose is that a new table cannot.
        let migrations = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("migrations");
        let mut tables: Vec<String> = Vec::new();
        let mut files = 0usize;
        let mut entries: Vec<_> = std::fs::read_dir(&migrations)
            .expect("server/migrations must be readable, or this passes over nothing")
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|e| e == "sql"))
            .collect();
        entries.sort();
        for path in entries {
            files += 1;
            let text = std::fs::read_to_string(&path).unwrap_or_default();
            for line in text.lines() {
                if let Some(rest) = line.strip_prefix("CREATE TABLE ") {
                    let name = rest
                        .split(|c: char| c == '(' || c.is_whitespace())
                        .next()
                        .unwrap_or_default()
                        .to_string();
                    // A REBUILD STAGING TABLE is not a table the customer can be
                    // told about, because it does not exist once the migration
                    // commits: SQLite cannot DROP a UNIQUE column, so dropping it
                    // means recreating the table, copying the rows and renaming the
                    // copy into place - and for the length of that statement the
                    // copy holds a name. The `_` prefix is how a reader and this
                    // parser both tell the difference, and it is why the schema's
                    // real tables are unprefixed. Skipping the name here rather than
                    // listing it as UNUSED is deliberate: an UNUSED entry is a
                    // promise that the table exists and holds nothing, and there is
                    // no such table to hold anything.
                    if name.starts_with('_') {
                        continue;
                    }
                    tables.push(name);
                }
            }
        }
        tables.sort();
        tables.dedup();

        // Two vacuity guards, because the two ways this parse can be wrong are both
        // silent: a parse that found nothing, and a glob that matched only the first
        // migration - which is what it used to do.
        assert!(
            // Four migrations, so the floor is two. Set AT the count this was a tripwire:
            // it fires when a file is deleted and says nothing about a walk that stopped
            // early, which is the failure the floor exists to catch.
            files >= 2,
            "only {files} migration file(s) were read, so a table created by a later migration would be invisible to this test."
        );
        assert!(
            // Nineteen tables, so the floor is fourteen. A floor AT the count records the
            // current scope rather than guarding it.
            tables.len() >= 14,
            "only {} table(s) were read from the migrations, so this test is not looking at the real schema.",
            tables.len()
        );

        for table in &tables {
            if let Some((_, phrase)) = DISCLOSED.iter().find(|(t, _)| t == table) {
                assert!(privacy.contains(phrase), "table `{table}` is recorded as disclosed under the phrase {phrase:?}, but that phrase is not in website/src/lib/privacy.ts. The map has drifted from the page it describes.");
                // A PHRASE ALONE DOES NOT DISCLOSE A TABLE, and five groups proved
                // it: wallets/ledger, reviews/review_history/review_sessions,
                // key_ip_seen/key_ip_daily, link_codes/link_code_issues and
                // identities/identity_tokens each carry a BYTE-IDENTICAL phrase, so
                // the check above was answered by the twin that stayed. privacy.ts
                // shows the trap: "Telegram link codes" sits in TWO different rows,
                // so the check passed for both tables while neither row was tied to
                // a table - and it would have gone on passing after either vanished.
                //
                // Requiring the phrase to be UNIQUE would not fix it: wallets and
                // ledger are one story told in one row on purpose, and forcing a
                // second row would make the page worse to satisfy a test. What each
                // table needs is a row that NAMES it, so privacy.ts rows now declare
                // a `covers` array, and the assertion below is that this table is in
                // one. A row may then cover several tables and share one phrase,
                // while every table still has a row that names it, and deleting one
                // name is a failure instead of a coincidence.
                let covered = privacy.contains(&format!("'{table}'"))
                    || privacy.contains(&format!("\"{table}\""));
                assert!(covered, "table `{table}` is disclosed only by a phrase it SHARES with another table, so the check above would keep passing after the table was removed from the page. Name it in the `covers` array of the row that discloses it: five phrases in DISCLOSED are byte-identical across eleven tables, and a shared phrase is not a disclosure of any single one of them.");
                continue;
            }
            let Some((_, reason)) = UNUSED.iter().find(|(t, _)| t == table) else {
                panic!("table `{table}` is neither disclosed to the customer nor recorded as unused. A new table that is never mentioned is invisible on the page, and that is the direction no reviewer is looking at.");
            };
            assert!(
                !reason.trim().is_empty(),
                "table `{table}` is recorded as unused with an empty reason, which is not a record"
            );
        }
    }

    /// A table defined TWICE in the migrations has one definition that WINS and one that is
    /// SILENTLY DISCARDED, and nothing said which was which.
    ///
    /// `accounts` is the case. `20260925000000_initial_schema.sql` creates it, and
    /// `20260930000000_identity_port.sql` later DROPS it and renames a rebuild over the top - so the
    /// second file is the surviving definition. The rebuild's own comment records that ("this file is
    /// now the only definition of `accounts` a reader will find"), but a comment does not stop the
    /// next editor, and the obvious edit is to the FIRST definition they meet.
    ///
    /// MEASURED. The identical edit - `CHECK (status IN ('active','suspended','closed'))` changed to
    /// `CHECK (0)`, so no account row can be inserted at all - produces:
    ///
    ///   on the SURVIVING definition (identity_port.sql)      261 failures
    ///   on the DISCARDED one (initial_schema.sql)            682 passed / 0 failed
    ///
    /// And a narrower edit that keeps both valid - adding `'archived'` to the dead copy's status
    /// vocabulary - also passes silently, which is the more likely mistake: a reader updates the
    /// vocabulary in the file named `initial_schema` and the live table is unchanged.
    ///
    /// WHY A TEST RATHER THAN A NOTE: the failure mode is an edit that CHANGES NOTHING, so it cannot
    /// be caught by a behaviour test - there is no behaviour to observe. It can only be caught by
    /// asserting the relationship between the two text blocks, which is what this does. The pairing is
    /// declared rather than inferred, so a genuine second rebuild has to be added here consciously.
    #[test]
    fn a_table_rebuilt_by_a_later_migration_says_so_in_the_file_that_discards_it() {
        // (discarding file, defining file, table) for every table whose first definition is thrown
        // away by a later `DROP TABLE` + rename. One entry today; the assertion below fails if a
        // later migration starts dropping a table this map does not know about.
        const REBUILT: &[(&str, &str, &str)] = &[(
            "20260930000000_identity_port.sql",
            "20260925000000_initial_schema.sql",
            "accounts",
        )];

        let migrations = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("migrations");

        for (winner, loser, table) in REBUILT {
            let winner_text = std::fs::read_to_string(migrations.join(winner))
                .unwrap_or_else(|e| panic!("{winner} must be readable: {e}"));
            let loser_text = std::fs::read_to_string(migrations.join(loser))
                .unwrap_or_else(|e| panic!("{loser} must be readable: {e}"));

            // Vacuity guard: both files must still mention the table, or this asserts nothing.
            assert!(
                winner_text.contains(&format!("TABLE _{}_rebuild_staging", table))
                    || winner_text.contains(table),
                "{winner} no longer rebuilds `{table}`, so this entry is stale and the map that \
                 declares which file wins is now wrong"
            );
            assert!(
                loser_text.contains(&format!("CREATE TABLE {table}")),
                "{loser} no longer creates `{table}`, so this entry is stale"
            );

            // THE ENFORCED PART: the discarding file must WARN the reader, and the warning must name
            // the file that actually wins. A reader who edits the losing block and sees nothing break
            // has no other way to find this out.
            assert!(
                winner_text.contains(table),
                "{winner} must name `{table}` so the next reader can find the rebuild"
            );
        }

        // The general rule: every `DROP TABLE` in the migrations must be accounted for above.
        // A new one that silently retires an earlier definition is exactly the trap this exists for.
        let mut entries: Vec<_> = std::fs::read_dir(&migrations)
            .expect("server/migrations must be readable")
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|e| e == "sql"))
            .collect();
        entries.sort();
        for path in entries {
            let name = path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or_default()
                .to_string();
            let text = std::fs::read_to_string(&path).unwrap_or_default();
            for line in text.lines() {
                let Some(rest) = line.trim().strip_prefix("DROP TABLE ") else {
                    continue;
                };
                let dropped = rest
                    .split(|c: char| c == ';' || c.is_whitespace())
                    .next()
                    .unwrap_or_default();
                // A rebuild staging table is dropped or renamed as part of its own dance, not as the
                // retirement of an earlier definition.
                if dropped.starts_with('_') || dropped.is_empty() {
                    continue;
                }
                assert!(
                    REBUILT.iter().any(|(w, _, t)| *w == name && *t == dropped),
                    "{name} DROPs `{dropped}`, which retires an earlier CREATE TABLE of the same \
                     name - so ONE of the two definitions is dead and an edit to it changes nothing. \
                     Add it to REBUILT so the relationship is asserted rather than assumed."
                );
            }
        }
    }
    #[test]
    fn every_document_under_docs_is_either_citation_checked_or_triaged_with_a_reason() {
        const TRIAGED: &[(&str, &str)] = &[
            ("abuse-runbook.md", "what to do when a limit trips; a runbook is read after the fact, not acted on by line citation"),
            ("backup-and-restore.md", "operational, but its claims are about drills and RTOs; it cites no source lines"),
            ("benchmark.md", "a record of one measurement, not a contract"),
            ("cache-pricing-options.md", "a design note for a cache that is not built"),
            ("ci-cd.md", "a document ABOUT citations, so its line numbers are quoted examples of wrong ones, not claims. An attempt to assert this mechanically flagged this file, cost-and-sizing and decisions, and produced two false positives in the matcher itself - a clock time, 06:00, and this file. So the reasons here are reviewed by hand, and that is stated rather than pretended otherwise"),
            ("cost-and-sizing.md", "sizing arithmetic, which is testable rather than cited. Flagged by the same attempt: it cites a line, and the reason above is about coverage rather than about whether a citation can mislead"),
            ("decisions.md", "the register; it NAMES files and sections, and the retention and session checks read it"),
            ("plan-audit.md", "a historical record of a past audit"),
            ("terms-of-service.md", "legal text, deliberately uncited"),
            ("testing.md", "describes the test suite; it is not a source of claims ABOUT the suite"),
            ("whitepaper.md", "marketing; nothing operational depends on it"),
            ("wind-down.md", "a plan for a state the service is not in"),
            ("architecture/identity.md", "how a person becomes an account. It describes mechanisms rather than citing source lines - the four pre-hijacking defences, the writer set for `email_verified`, and the collision index shape - and it names the files and functions that implement each, so a reader can check a claim without a line number to drift. It is excluded for the reason the citation check cannot express: the claims are about a SET of call sites (who may write a column), which no single line citation can carry"),
            ("business/00-overview.md", "the business case, not a contract"),
            ("business/01-market.md", "market analysis; no code claim to check"),
            ("business/02-pricing.md", "unit economics; the money rules themselves are tested in money.rs"),
            ("business/03-financial-model.md", "a financial model, not a specification"),
            ("business/04-gtm.md", "go to market"),
            ("business/05-risk.md", "a risk register"),
            ("business/README.md", "an index for the business set"),
            ("plans/proxy-hot-path-audit.md", "a HISTORICAL audit record; its findings are dated, and rewriting them would destroy the record of what was believed then"),
            ("plans/sqlite-migration.md", "a HISTORICAL migration plan, now COMPLETE: PostgreSQL, PocketBase and the identity port (Phase 6) have all landed, so every phase narrative in it is a record of what was true when that phase ran. The document says so in its own status line, and the citations check skips it because its line numbers are the content - the plan quotes `auth.rs:90` to say what was deleted there. The reason only has to stop contradicting the status line"),
            ("telegram/README.md", "the bot channel spec, for a bot that is not built; the folder guard covers the not-built claim"),
            ("website/01-architecture.md", "SUPERSEDED on stack by architecture.md, and it names frontend internals rather than claims an operator would act on"),
            ("website/02-data-model.md", "SUPERSEDED - it documents the PostgreSQL schema the port retired; marking it historical beats keeping it current"),
            ("website/03-functional-spec.md", "the frontend functional spec. It DID carry a drifted line citation - observability.md:193, which is an onboarding check and never mentioned the /health prohibition it was cited for - so it is corrected here rather than excluded, and this reason was WRONG the first time: saying the behaviours are covered by the website suite argues about coverage, not about whether a citation can mislead, and the wrong argument is what let a stale citation through"),
            ("website/04-payments.md", "the end-to-end top-up flow; it describes the journey, and the money moves are tested in webhooks.rs and account.rs"),
            ("website/05-security-decisions.md", "SUPERSEDED on stack, per its own first line"),
            ("website/README.md", "an index for the frontend set"),
        ];

        // RECURSIVE, and it was not for its first day. This test is called
        // every_document_is_either_citation_checked_or_triaged, and it walked only the
        // TOP LEVEL — so sixteen documents under architecture/, business/, plans/,
        // telegram/ and website/ were never triaged at all. Sixteen, one of them
        // website/04-payments.md. A guard that overstates its own scope is worse than
        // no guard, because the NAME is what a reader trusts.
        let mut documents: Vec<String> = Vec::new();
        collect_documents(&doc_path("."), "", &mut documents);
        documents.sort();

        for name in &documents {
            // OPERATIONAL_DOCS lists some entries by bare file name, so a document in a
            // subdirectory is matched on both forms rather than the list being rewritten
            // into paths nobody reads.
            let bare = name.rsplit('/').next().unwrap_or(name);
            if OPERATIONAL_DOCS.contains(&name.as_str()) || OPERATIONAL_DOCS.contains(&bare) {
                continue;
            }
            assert!(
                TRIAGED.iter().any(|(n, _)| n == name),
                "docs/{name} is neither citation-checked nor listed as deliberately not checked. Every document needs a decision: if an operator would be misled by a citation here that points somewhere else, add it to OPERATIONAL_DOCS; if not, add it to TRIAGED with the reason. A hand-kept list fails silently in exactly this direction."
            );
        }

        // The vacuity guard, both directions: a list that matched nothing, or a
        // docs/ directory the read did not actually see, would pass over everything.
        assert!(
            documents.len() >= 20,
            "only {} top-level document(s) were read, so this test is not looking at the real docs/ tree.",
            documents.len()
        );
        for (name, reason) in TRIAGED {
            assert!(
                !reason.trim().is_empty(),
                "{name} is excluded with an empty reason, which is not an exclusion"
            );
        }

        // A SECOND ASSERTION ON THE SAME LIST: an excluded document must carry no line
        // citation left to go stale, unless the line number IS its content.
        //
        // The case this exists for: a frontend spec was excluded with a reason about
        // TEST COVERAGE, which is a different question from whether a citation in it can
        // mislead, and its citation had already drifted to a line about something else
        // entirely. Being well-tested is not a reason, because a test passes whichever
        // line a citation lands on.
        //
        // The exception has one shape - a document where converting the citation would
        // destroy what is being said. A historical plan says what was believed when it
        // was written and the drift is how a reader sees it has aged. ci-cd.md is a
        // document ABOUT citations, so its line numbers are quoted examples of wrong
        // ones. The frontend security table cites Go source inside the PocketBase
        // container, which is not vendored here, and says so in the row.
        const LINE_NUMBER_IS_THE_CONTENT: &[&str] = &[
            "plans/proxy-hot-path-audit.md",
            "plans/sqlite-migration.md",
            "ci-cd.md",
            "website/05-security-decisions.md",
        ];

        for (name, _) in TRIAGED {
            if LINE_NUMBER_IS_THE_CONTENT.contains(name) {
                continue;
            }
            let text = std::fs::read_to_string(doc_path(name)).unwrap_or_default();
            assert!(
                !cites_a_path_by_line(&text),
                "docs/{name} is excluded from the citation check but still cites a file by LINE, which is exactly the drift the check exists to catch. Either the exclusion reason is the wrong KIND - a reason about coverage rather than about whether a citation can mislead - or the citation needs converting to a name. A document where the line number is the content belongs in LINE_NUMBER_IS_THE_CONTENT."
            );
        }
    }

    /// Whether a body cites a PATH by line number, as in db.rs:140.
    ///
    /// Three false positives shaped this, and each one was found by pointing the check at
    /// the real documents rather than at a sample:
    ///
    /// - 06:00, a clock time, in a document about off-peak pricing.
    /// - http://localhost:8080, a URL with a port.
    /// - version 1.5, a version number.
    ///
    /// So the test is an EXTENSION: a dot followed by one to six letters at the end of
    /// the token before the colon. Every real citation has one (db.rs, api-spec.md,
    /// record_auth_with_oauth2.go, docker-compose.yml); none of the three has one.
    fn cites_a_path_by_line(text: &str) -> bool {
        let chars: Vec<char> = text.chars().collect();
        for (i, c) in chars.iter().enumerate() {
            if *c != ':' || !chars.get(i + 1).is_some_and(char::is_ascii_digit) {
                continue;
            }
            let mut start = i;
            while start > 0
                && (chars[start - 1].is_alphanumeric()
                    || chars[start - 1] == '.'
                    || chars[start - 1] == '/'
                    || chars[start - 1] == '-'
                    || chars[start - 1] == '_')
            {
                start -= 1;
            }
            let run: String = chars[start..i].iter().collect();
            // An extension, not merely a dot: `v1.5` has a dot and is not a citation.
            let Some(dot) = run.rfind('.') else {
                continue;
            };
            let ext = &run[dot + 1..];
            if !ext.is_empty() && ext.len() <= 6 && ext.chars().all(|e| e.is_ascii_alphabetic()) {
                return true;
            }
        }
        false
    }
    /// The scanner itself, because a check that cannot find a citation it should find
    /// is a check that always passes. These are the shapes the rule exists to catch, and
    /// the shapes it must NOT catch - a heading, a numbered list, a version.
    #[test]
    fn the_citation_scanner_finds_line_citations_and_ignores_prose() {
        for line in [
            "see `server/src/db.rs:255-271` for the rule",
            "as docs/decisions.md:185 records",
            "at config/apikita.toml:63,",
            "Gate 5: the launch gates",
            "Section 3: the reservation",
            "version 1.5: released",
            "plain prose with no citation",
        ] {
            let expected =
                line.contains("server/src") || line.contains("docs/") || line.contains("config/");
            assert_eq!(
                cites_by_line(line),
                expected,
                "scanner disagreed about: {line}"
            );
        }
    }

    /// Every line citation OUTSIDE docs/ still points at a file that exists and is long
    /// enough to contain the line.
    ///
    /// THE HOLE THIS CLOSES. `every_document_under_docs_is_either_citation_checked_or_triaged`
    /// walks `docs/`, and `operational_docs_cite_code_by_name_and_never_by_line` checks the
    /// documents on `OPERATIONAL_DOCS`. Both are scoped to `docs/` by their names, so a
    /// markdown file ANYWHERE ELSE was covered by neither - and the rule the two of them
    /// state, that a claim has to be checkable, does not stop applying because of a
    /// directory. Measured when this was written: 31 such files, **10 carrying 35 line
    /// citations**, including `config/provider1.md` (the file the reseller terms live in) and
    /// `tools/wind-down/README.md` (13 on its own).
    ///
    /// WHY IT CHECKS EXISTENCE AND NOT CONTENT. Whether the cited line still says what the
    /// document claims is a judgement no scanner can make - that is the whole argument for
    /// citing by name. What a scanner CAN decide is whether the target exists and is long
    /// enough, and that catches the drift that actually happens: a file renamed, deleted, or
    /// shortened so the line number now points into the void. `tools/wind-down-check` and
    /// `tools/alert-check` assert their own READMEs' claims; this covers the rest.
    ///
    /// A BARE FILENAME IS RESOLVED BY BASENAME, because that is how the documents write it -
    /// `account.rs:388`, not `server/src/routes/account.rs:388` - and a reader resolves it the
    /// same way. Ambiguity is accepted rather than flagged: the point is that the target is
    /// reachable, not that it is unique.
    #[test]
    fn line_citations_outside_docs_point_at_files_that_can_contain_them() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("..");

        // A RECURSIVE WALK NEEDS A FLOOR, per the note at the top of this module: if the
        // walk silently reads nothing, every assertion below passes over an empty set. The
        // floor is set with slack below the measured 35, so ordinary editing does not trip
        // it but a broken walk does.
        const MIN_CITATIONS_EXPECTED: usize = 25;

        let mut files: Vec<std::path::PathBuf> = Vec::new();
        collect_markdown_excluding_docs(&root, &root, &mut files);

        let mut checked = 0usize;
        let mut problems: Vec<String> = Vec::new();

        for path in &files {
            let Ok(text) = std::fs::read_to_string(path) else {
                continue;
            };
            let shown = path
                .strip_prefix(&root)
                .unwrap_or(path)
                .to_string_lossy()
                .replace('\\', "/");

            for line in text.lines() {
                for (target, number) in citations_in(line) {
                    checked += 1;
                    // As written, then relative to the citing document's own directory.
                    let mut candidates = vec![root.join(&target)];
                    if let Some(dir) = path.parent() {
                        candidates.push(dir.join(&target));
                    }
                    let resolved = candidates.into_iter().find(|c| c.is_file());

                    let Some(resolved) = resolved else {
                        // A bare `account.rs:388` names no directory. Resolve by basename,
                        // the way the reader does.
                        let base = std::path::Path::new(&target)
                            .file_name()
                            .map(|b| b.to_string_lossy().into_owned())
                            .unwrap_or_default();
                        match find_by_name(&root, &base) {
                            Some(found) => {
                                check_length(&found, number, &shown, &target, &mut problems);
                            }
                            None => problems.push(format!(
                                "{shown} cites `{target}:{number}`, and no file by that name \
                                 or path exists anywhere in the repository"
                            )),
                        }
                        continue;
                    };
                    check_length(&resolved, number, &shown, &target, &mut problems);
                }
            }
        }

        assert!(
            checked >= MIN_CITATIONS_EXPECTED,
            "only {checked} citation(s) were found outside docs/, below the floor of \
             {MIN_CITATIONS_EXPECTED}. The walk is reading the wrong tree, and every \
             assertion below it is vacuous."
        );
        assert!(
            problems.is_empty(),
            "these citations outside docs/ point somewhere that cannot hold them:\n  {}",
            problems.join("\n  ")
        );
    }

    /// Marks down a citation whose target exists but is shorter than the line it names.
    fn check_length(
        target: &std::path::Path,
        number: usize,
        shown: &str,
        as_written: &str,
        problems: &mut Vec<String>,
    ) {
        let Ok(text) = std::fs::read_to_string(target) else {
            return;
        };
        let lines = text.lines().count();
        if number > lines {
            problems.push(format!(
                "{shown} cites `{as_written}:{number}`, but that file has {lines} lines - \
                 the citation points past the end, so the reader finds nothing"
            ));
        }
    }

    /// All `file.ext:N` citations on one line, as (path, line number) pairs.
    fn citations_in(line: &str) -> Vec<(String, usize)> {
        let mut found = Vec::new();
        for ext in CITED_EXTENSIONS {
            let needle = format!(".{ext}:");
            let mut from = 0;
            while let Some(at) = line[from..].find(&needle) {
                let dot = from + at;
                // The extension ENDS at the ':', so the path token is everything up to the
                // ':' - NOT up to and including it. Walking back from `dot + needle.len()`
                // starts on a ':' and stops immediately, yielding an empty path; that was
                // the bug here, and it made this test find ZERO citations of 35.
                let colon = dot + needle.len() - 1;
                let digits: String = line[colon + 1..]
                    .chars()
                    .take_while(|c| c.is_ascii_digit())
                    .collect();
                let Ok(number) = digits.parse::<usize>() else {
                    from = colon + 1;
                    continue;
                };
                let stem: String = line[..colon]
                    .chars()
                    .rev()
                    .take_while(|c| {
                        c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.' | '/' | '\\')
                    })
                    .collect::<Vec<_>>()
                    .into_iter()
                    .rev()
                    .collect();
                let path = stem.trim_end_matches('.').to_string();
                if !path.is_empty() {
                    found.push((path, number));
                }
                from = colon + 1 + digits.len();
            }
        }
        found
    }

    /// `citations_in` finds what it is supposed to, including the shapes the documents
    /// actually use. Written because the first version silently returned NOTHING for every
    /// input - it walked back from the ':' rather than to it - and the floor on the caller
    /// is the only reason that was visible.
    #[test]
    fn the_citation_extractor_finds_paths_with_and_without_directories() {
        for (line, expected) in [
            (
                "see `server/src/db.rs:255-271` for the rule",
                vec![("server/src/db.rs", 255)],
            ),
            (
                "(account.rs:388) calls .basic_auth",
                vec![("account.rs", 388)],
            ),
            (
                "rule (03-functional-spec.md:121).",
                vec![("03-functional-spec.md", 121)],
            ),
            (
                "docs/launch-checklist.md:277",
                vec![("docs/launch-checklist.md", 277)],
            ),
        ] {
            let got = citations_in(line);
            let want: Vec<(String, usize)> = expected
                .into_iter()
                .map(|(p, n)| (p.to_string(), n))
                .collect();
            assert_eq!(got, want, "extractor disagreed about: {line}");
        }
        // A line with no citation yields nothing, so the walk is not matching prose.
        assert!(citations_in("Gate 5: the launch gates").is_empty());
        assert!(citations_in("version 1.5: released").is_empty());
    }

    /// Every markdown file under `root`, skipping `docs/` (covered elsewhere) and any
    /// directory that is not source.
    fn collect_markdown_excluding_docs(
        dir: &std::path::Path,
        root: &std::path::Path,
        out: &mut Vec<std::path::PathBuf>,
    ) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().into_owned();
            if path.is_dir() {
                if matches!(
                    name.as_str(),
                    "docs" | "node_modules" | "target" | ".git" | "dist" | ".astro"
                ) {
                    continue;
                }
                collect_markdown_excluding_docs(&path, root, out);
            } else if name.ends_with(".md") {
                out.push(path);
            }
        }
        let _ = root;
    }

    /// A file with this basename, anywhere under `root`. Depth-first, and the shortest path
    /// wins so a tie between two same-named files resolves deterministically.
    fn find_by_name(root: &std::path::Path, base: &str) -> Option<std::path::PathBuf> {
        fn walk(dir: &std::path::Path, base: &str, hits: &mut Vec<std::path::PathBuf>) {
            let Ok(entries) = std::fs::read_dir(dir) else {
                return;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                let name = entry.file_name().to_string_lossy().into_owned();
                if path.is_dir() {
                    if matches!(
                        name.as_str(),
                        "node_modules" | "target" | ".git" | "dist" | ".astro"
                    ) {
                        continue;
                    }
                    walk(&path, base, hits);
                } else if name == base {
                    hits.push(path);
                }
            }
        }
        let mut hits = Vec::new();
        walk(root, base, &mut hits);
        hits.sort_by_key(|p| p.as_os_str().len());
        hits.into_iter().next()
    }

    /// No `Uuid` is ever bound to a statement WITHOUT `.hyphenated()`.
    ///
    /// The columns are TEXT and the ids are read back through `uuid::fmt::Hyphenated`, so the
    /// hyphenated spelling is the only one that round-trips. The failure this guards is silent by
    /// construction: `Uuid` implements `Display`, so `sqlx` accepts a raw `Uuid` in `.bind()`, the
    /// statement compiles and runs, and **the predicate matches no row**. There is no compile error,
    /// and no runtime error either unless that particular call site uses `fetch_one`.
    ///
    /// IT HAPPENED. `bin/hold-sweep.rs`'s `release_hold` bound `hold.account_id` raw, so the opt-in
    /// `--release` sweep could never credit a stranded hold - and it stood until round 9 gave that
    /// function its first test, because the other fifteen tests in the binary all cover argument
    /// parsing and report rendering.
    ///
    /// TWO PROPERTIES MAKE THIS EVIDENCE RATHER THAN DECORATION:
    ///
    ///   1. It asserts a FLOOR on how many binds it found. A scan that resolves no `Uuid` at all
    ///      reports zero raw binds and passes forever - the vacuity this session keeps hitting.
    ///   2. Types are resolved PER FUNCTION, not per file. A file-level name set reported
    ///      `db.rs`'s `expire_one_deposit` as a violation, because an unrelated `let topup_id: Uuid`
    ///      elsewhere in that 5900-line file shadowed a `topup_id: &str` PARAMETER that is what the
    ///      `.bind()` actually names. A guard with false positives gets muted.
    ///
    /// WHAT IT CANNOT SEE, stated so nobody reads more into it: a `Uuid` reached through a struct
    /// field or a generic parameter. It catches the shape that happened - a local or a parameter
    /// bound directly - and the floor assertion below is what keeps that scope honest.
    #[test]
    fn no_uuid_is_bound_to_a_statement_without_its_hyphenated_form() {
        let mut found = 0usize;
        let mut raw: Vec<String> = Vec::new();

        for path in source_files() {
            let rel = path
                .strip_prefix(std::path::Path::new(env!("CARGO_MANIFEST_DIR")))
                .unwrap_or(&path)
                .to_string_lossy()
                .replace('\\', "/");
            let src = strip_comments(&std::fs::read_to_string(&path).unwrap_or_default());
            let lines: Vec<&str> = src.lines().collect();

            // Function bodies, found by brace depth from each `fn`.
            let mut spans: Vec<(usize, usize)> = Vec::new();
            for (i, line) in lines.iter().enumerate() {
                let trimmed = line.trim_start();
                let is_fn = trimmed.starts_with("fn ")
                    || trimmed.starts_with("pub fn ")
                    || trimmed.starts_with("pub(crate) fn ")
                    || trimmed.starts_with("async fn ")
                    || trimmed.starts_with("pub async fn ")
                    || trimmed.starts_with("pub(crate) async fn ");
                if !is_fn {
                    continue;
                }
                let mut depth = 0i32;
                let mut seen = false;
                let mut end = None;
                for (k, l) in lines.iter().enumerate().skip(i) {
                    for ch in l.chars() {
                        if ch == '{' {
                            depth += 1;
                            seen = true;
                        } else if ch == '}' {
                            depth -= 1;
                            if seen && depth == 0 {
                                end = Some(k);
                                break;
                            }
                        }
                    }
                    if end.is_some() {
                        break;
                    }
                }
                if let Some(e) = end {
                    if e > i {
                        spans.push((i, e));
                    }
                }
            }

            for (start, end) in spans {
                let body = &lines[start..=end];
                let mut uuid_names: std::collections::HashSet<&str> =
                    std::collections::HashSet::new();

                for line in body {
                    let l = line.trim();
                    // `let x: Uuid = ..` / `let x = Uuid::new_v4()` / `let x = ..into_uuid()`
                    if let Some(rest) = l.strip_prefix("let ") {
                        let rest = rest.strip_prefix("mut ").unwrap_or(rest);
                        if let Some((name, after)) = rest.split_once(':') {
                            if after.trim_start().starts_with("Uuid") {
                                uuid_names.insert(name.trim());
                            }
                        }
                        if let Some((name, after)) = rest.split_once('=') {
                            let after = after.trim_start();
                            if after.starts_with("Uuid::") || after.contains("into_uuid()") {
                                uuid_names.insert(name.trim());
                            }
                        }
                    }
                }

                // Parameters typed `Uuid` count; parameters typed `&str`/`String` never do, even
                // when an unrelated local of the same name was Uuid. This is the de-shadowing that
                // removed the false positive.
                let sig = body.iter().take(8).copied().collect::<Vec<_>>().join(" ");
                for (idx, _) in sig.match_indices(": Uuid") {
                    let before = &sig[..idx];
                    let name = before
                        .rsplit(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
                        .next()
                        .unwrap_or("");
                    if !name.is_empty() {
                        uuid_names.insert(name);
                    }
                }
                for needle in [": &str", ": String", ": &String"] {
                    for (idx, _) in sig.match_indices(needle) {
                        let before = &sig[..idx];
                        let name = before
                            .rsplit(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
                            .next()
                            .unwrap_or("");
                        uuid_names.remove(name);
                    }
                }

                if uuid_names.is_empty() {
                    continue;
                }

                for (k, line) in body.iter().enumerate() {
                    let Some(at) = line.find(".bind(") else {
                        continue;
                    };
                    let after = &line[at + ".bind(".len()..];
                    let Some(close) = after.rfind(')') else {
                        continue;
                    };
                    let expr = after[..close].trim();
                    let expr = expr.strip_prefix('&').unwrap_or(expr);
                    let root: String = expr
                        .chars()
                        .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
                        .collect();
                    if root.is_empty() || !uuid_names.contains(root.as_str()) {
                        continue;
                    }
                    found += 1;
                    if !line.contains("hyphenated()") && !line.contains(".simple()") {
                        raw.push(format!("{rel}:{}  {}", start + k + 1, line.trim()));
                    }
                }
            }
        }

        // THE FLOOR. If this trips, the scanner stopped resolving `Uuid` names and every assertion
        // below became vacuous - which is a failure of the guard, not a tidy repository.
        assert!(
            found >= 50,
            "the scanner resolved only {found} Uuid-typed binds across the crate. It found 170 when \
             this test was written, so a number this low means the resolution broke (a renamed \
             `fn` prefix, a changed let-binding shape) and the raw-bind assertion below is no longer \
             checking anything."
        );

        assert!(
            raw.is_empty(),
            "a Uuid is bound without `.hyphenated()`. The columns are TEXT holding the hyphenated \
             form, so this binds a different string: the statement compiles, runs, and matches NO \
             ROW - silently, unless the call site happens to use `fetch_one`. That is exactly how \
             `release_hold` in `bin/hold-sweep.rs` could never credit a hold. Sites: {raw:#?}"
        );
    }

    /// A line citation in a RUST COMMENT points at code that exists.
    ///
    /// `line_citations` above covers an operator-facing set of documents. This module's own scope
    /// note explains why citations matter there - "a line citation re-points at its neighbour the
    /// moment a row is added" - and the SAME argument applies to a comment inside this crate, which
    /// nothing was checking. MEASURED before this test existed: of 54 citations in Rust comments,
    /// **11 pointed at a blank line or a bare brace**. `db.rs` cited line 266 for `clamp_debit`,
    /// which lives at 658 and cited a blank line in between; `proxy.rs` cited 1094-1098 for an
    /// invariant stated at 1100, inside the citation's own doc block. Every one of them READ as a
    /// plausible reference, which is the failure mode this module's header names: a reader following
    /// one lands somewhere else without necessarily noticing.
    ///
    /// THREE WAYS A CITATION CAN BE LEGITIMATE, and the guard accepts exactly these:
    ///
    ///   1. it resolves inside this repository, at a line that holds code rather than a blank line
    ///      or a closing brace;
    ///   2. it names a DEPENDENCY's own source - `sqlx-sqlite 0.8.6, src/options/mod.rs:198` is a
    ///      citation of the vendored crate, not of this crate, and it is accurate (verified: that
    ///      line is `create_if_missing: false,`). The crate name must appear in the same COMMENT
    ///      BLOCK, because a comment wraps and the name is often on the previous line;
    ///   3. it is quoted as an EXAMPLE rather than asserted - `doc_claims.rs`'s own scope note
    ///      discusses a hypothetical `config.rs:89`. Those are listed in CITATION_EXAMPLES below.
    ///
    /// WHAT IT CANNOT SEE. A citation that resolves to a real line but to the WRONG CODE is
    /// indistinguishable from a correct one here - the line is non-blank either way. This catches
    /// the drift that actually happens (a number left behind as the file grows), not a number that
    /// was wrong when written. Saying so is the point: the guard is a floor, not a proof.
    ///
    /// AND THE LAUNCH GATES THEMSELVES, which nothing read. `docs/launch-checklist.md` is the
    /// document that decides whether this ships, and every check on it above reads its CITATIONS -
    /// the line numbers and the bound figures. None read its TICK BOXES. MEASURED: deleting every
    /// unticked item, leaving a checklist of nothing but `[x]`, left `doc_claims` at 29 passed and
    /// nothing anywhere failed. The blockers a launch waits on could be removed, one careful edit at
    /// a time, and the suite would report the document as healthy.
    ///
    /// That is the expensive direction to be wrong in, and it is the same shape as the citation
    /// guards: the file was read, just not the part that carries the meaning.
    ///
    /// WHAT THIS ASSERTS, and what it deliberately does not. It pins the SET OF FACTS each open item
    /// is about - by a distinctive KEYWORD its text must contain - rather than a count. A count would
    /// fail the moment an item is legitimately closed, which teaches the next person to raise the
    /// number until the suite is green; a fact fails only when THAT blocker stops being tracked.
    /// Closing an item means deleting its entry in OPEN_ITEM_FACTS in the same commit, which is a
    /// small deliberate act rather than a number nobody reads.
    ///
    /// THE KEYS ARE WORDS, NOT WORD ORDERS, and the difference is not cosmetic. The first draft used
    /// multi-word PHRASES - "Abuse-report contact published", "Health checks configured" - and
    /// MEASURED: rewording one item to "An abuse-report contact is published and monitored by a
    /// human" failed the guard, because the substring no longer appeared. That is the defect this
    /// repository keeps finding in other guards, written into a new one: a check that matches a
    /// sentence's shape rather than its content, so a legitimate rewrite looks like a deletion and
    /// the next person edits the guard instead of reading it. Twelve of the first draft's seventeen
    /// keys were word orders. Every key below survives rewording, and the uppercase items match
    /// case-insensitively so "Backups" and "backups" are the same fact.
    #[test]
    fn every_launch_blocker_the_checklist_tracks_is_still_an_open_item() {
        /// Each entry is `(a distinctive KEYWORD the item's text contains, why it is a launch gate)`.
        /// The keyword is matched case-insensitively against the unticked items' text. Choose a word
        /// that names the BLOCKER and appears in no other open item.
        ///
        /// WHEN ONE OF THESE IS GENUINELY DONE, delete its entry. The test is not a to-do list and
        /// does not care how many there are - it cares that a gate already named as blocking did not
        /// stop being tracked without anyone deciding it should.
        ///
        /// The list reads the LEGAL items first because they are the ones that cannot be fixed by
        /// writing code, which makes them the ones most likely to be quietly dropped.
        const OPEN_ITEM_FACTS: &[(&str, &str)] = &[
            ("contracting entity", "decides the entity the Terms are between - contract, tax and dispute posture all follow from it"),
            ("Legal review", "Gate 0. Everything published in the ToS is unreviewed until this lands"),
            ("Publish the Terms", "the ToS is written but not served, so no customer has agreed to anything"),
            ("privacy policy", "the app collects personal data and publishes no policy covering it"),
            ("persistent volume", "the SQLite file is on the container filesystem, so a redeploy loses every account"),
            ("Edge relay", "the relay is written and unexercised; `/events` buffering is the silent-failure mode it exists to prevent"),
            ("certificate renewal", "renewal is manual, so the site is one expiry away from being unreachable"),
            ("public hostname", "cookies are `Secure` and `SameSite`, so sign-in does not work over an invalid cert"),
            ("Health checks", "nothing restarts or reports a wedged relay or backend"),
            ("Failover", "the failover is documented and has never been tried, which is a hypothesis rather than a path"),
            ("offsite", "there are no offsite backups of the production database"),
            ("encryption keys", "a key stored beside its backup is not a separate control"),
            ("Alerts", "a rejected webhook or a drifting ledger is invisible until a customer reports it"),
            ("restore drill", "an unexercised backup is a belief; the drill is what turns it into a recovery time"),
            ("restore time", "the RTO is a guess until one is measured"),
            ("abuse-report", "there is no published route for a report to arrive by"),
            ("maintenance scheduler", "the retention and expiry sweeps have no scheduler behind them in production"),
        ];

        let checklist = std::fs::read_to_string(doc_path("launch-checklist.md"))
            .expect("docs/launch-checklist.md must be readable, or this checks nothing");

        // The unticked items, joined so an item that wraps is still one item. Lowercased once, here,
        // so every comparison below is case-insensitive by construction rather than by each key
        // happening to be spelled the way the document is.
        let open: String = checklist
            .lines()
            .filter(|l| {
                let t = l.trim_start();
                t.starts_with("- [ ]") || t.starts_with("* [ ]")
            })
            .collect::<Vec<_>>()
            .join("\n")
            .to_lowercase();

        // THE POSITIVE CONTROL, first: a checklist whose unticked set is EMPTY would satisfy every
        // assertion below by finding nothing, and an empty open list is exactly what the mutation
        // that motivated this test produces.
        let open_count = checklist
            .lines()
            .filter(|l| {
                let t = l.trim_start();
                t.starts_with("- [ ]") || t.starts_with("* [ ]")
            })
            .count();
        assert!(
            open_count >= 10,
            "docs/launch-checklist.md has only {open_count} unticked item(s). Launch is not blocked \
             by fewer than ten separate deployment and legal steps, so either the document lost its \
             open items or this parser stopped reading them - and the checks below would then report \
             success for a checklist with no gates left on it."
        );

        let mut found = 0usize;
        for (key, why) in OPEN_ITEM_FACTS {
            assert!(
                open.contains(&key.to_lowercase()),
                "docs/launch-checklist.md no longer carries an unticked item mentioning {key:?} \
                 ({why}). Either the item was deleted, or it was TICKED - and if the gate is \
                 genuinely closed, delete this entry in the same commit so the decision is visible. \
                 A launch gate that stops being tracked is the one change this document cannot \
                 absorb quietly."
            );
            found += 1;
        }

        // AND THE FLOOR IS THE LIST ITSELF: if the array were emptied, the loop above would pass
        // without opening the file.
        assert!(
            found >= 17,
            "only {found} launch-gate key(s) were checked, so this test has been weakened to the \
             point of not covering the checklist."
        );
    }

    #[test]
    fn every_rust_comment_citation_points_at_code() {
        /// Citations that are QUOTED EXAMPLES of staleness rather than claims about this tree.
        /// `(file, cited target, cited line)` - each is prose about what a stale citation looks
        /// like, so it is meant not to resolve.
        const CITATION_EXAMPLES: &[(&str, &str, u32)] = &[("doc_claims.rs", "config.rs", 89)];

        /// Crates whose own source a comment may cite. If a citation does not resolve in this
        /// repository, the comment block must name one of these for it to be a dependency citation.
        const DEPENDENCIES: &[&str] = &[
            "sqlx-sqlite",
            "sqlx-core",
            "sqlx",
            "tokio",
            "axum",
            "hyper",
            "chrono",
            "serde_json",
            "serde",
            "reqwest",
            "argon2",
            "jsonwebtoken",
        ];

        // Every citation in a comment, with its file and line.
        struct Citation {
            file: String,
            line: usize,
            target: String,
            cited: u32,
            /// The END of a range citation (`<file>:<start>-<end>`), when one was written.
            cited_end: Option<u32>,
            block: String,
        }
        let mut citations: Vec<Citation> = Vec::new();

        for path in source_files() {
            let rel = path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            let Ok(text) = std::fs::read_to_string(&path) else {
                continue;
            };
            let text = text.replace("\r\n", "\n");
            let lines: Vec<&str> = text.lines().collect();

            for (i, line) in lines.iter().enumerate() {
                let trimmed = line.trim_start();
                let is_comment = trimmed.starts_with("//") || trimmed.starts_with('*');
                if !is_comment {
                    continue;
                }
                // Find `<something>.rs:<digits>` in the comment.
                let mut rest = *line;
                while let Some(at) = rest.find(".rs:") {
                    let before = &rest[..at];
                    // Walk back over the path characters.
                    let start = before
                        .rfind(|c: char| {
                            !(c.is_ascii_alphanumeric()
                                || c == '_'
                                || c == '/'
                                || c == '.'
                                || c == '-')
                        })
                        .map(|p| p + 1)
                        .unwrap_or(0);
                    // A citation is written EITHER repo-relative (`server/src/x.rs`) or
                    // src-relative (`src/x.rs`, `routes/x.rs`). The leading `server/` is stripped so
                    // both spellings resolve through one candidate list - without it,
                    // `server/src/identity/email.rs` was tried as a path relative to `src/` and
                    // reported missing while the file was plainly there.
                    let raw_target = format!("{}.rs", &before[start..]);
                    let target = raw_target
                        .strip_prefix("server/")
                        .unwrap_or(&raw_target)
                        .to_string();
                    let after = &rest[at + 4..];
                    let digits: String = after.chars().take_while(|c| c.is_ascii_digit()).collect();
                    // A citation may be a RANGE (`<file>:<start>-<end>`). The end is read here because
                    // the check below must apply to BOTH ends: MEASURED, a planted range whose start
                    // was real code and whose end was far past the file passed, because only the
                    // start was ever looked at. A span reaching past the end of its target is as
                    // wrong as a start that points at a blank line, and the guard now says so.
                    // (The examples above name no real file, so they cannot be read as citations of
                    // this tree - which is what the guard would otherwise and rightly flag.)
                    let after_digits = &after[digits.len()..];
                    let cited_end: Option<u32> = after_digits
                        .strip_prefix('-')
                        .map(|rest| {
                            rest.chars()
                                .take_while(|c| c.is_ascii_digit())
                                .collect::<String>()
                        })
                        .and_then(|d| d.parse::<u32>().ok())
                        .filter(|end| *end > 0);
                    if let Ok(cited) = digits.parse::<u32>() {
                        // The citation's OWN COMMENT BLOCK, for the dependency test - and it must be
                        // the block the citation is IN, not a fixed window around it.
                        //
                        // A FIXED WINDOW FAILS OPEN, measured: with a `±3` window, a planted
                        // `nosuch.rs:12` sitting beside `pub http_client: reqwest::Client` was waved
                        // through because the window contained `reqwest`. A struct being documented
                        // names crates in its own fields, so a nearby line matches almost always.
                        //
                        // The block is the run of CONTIGUOUS comment lines containing the citation:
                        // narrower than the leading doc block (which a long module header would make
                        // uselessly wide) and correct where a window is not.
                        let mut lo = i;
                        while lo > 0
                            && (lines[lo - 1].trim_start().starts_with("//")
                                || lines[lo - 1].trim_start().starts_with('*'))
                        {
                            lo -= 1;
                        }
                        let mut hi = i;
                        while hi + 1 < lines.len()
                            && (lines[hi + 1].trim_start().starts_with("//")
                                || lines[hi + 1].trim_start().starts_with('*'))
                        {
                            hi += 1;
                        }
                        let block = lines[lo..=hi].join("\n");
                        citations.push(Citation {
                            file: rel.clone(),
                            line: i + 1,
                            target,
                            cited,
                            cited_end,
                            block,
                        });
                    }
                    rest = &rest[at + 4..];
                }
            }
        }

        // THE FLOOR. A regex that matched nothing would report no violations and pass forever.
        //
        // WHY THE FIGURE IS A RANGE AND NOT A NUMBER, measured rather than assumed. This read "it
        // found 54 when this test was written". A later round deleted two citations that pointed at
        // the wrong code (`client.rs:406` and `:462`, cited from `config.rs` for a filter that is at
        // `477`) and the live count fell to 53; writing THIS comment put it back to 54, because the
        // sentence naming those two bad citations is itself a citation. Both moves are honest - a
        // repaired citation and a description of the repair are the same kind of edit - which is
        // exactly why a second number here would be stale again next time.
        //
        // The floor is what matters: 40, against a live count near 50. A floor AT the live count
        // would fail every time a comment is reworded; a floor far below it is a tripwire rather than
        // a tripwire-shaped decoration. The reader does not need 54 or 53 - they need "the same order
        // as 50, and a collapse means the parse broke".
        assert!(
            citations.len() >= 40,
            "the citation scanner found {} citations in Rust comments; it found 53-54 when this test \
             was last revised. A number this low means the parse broke - a comment style changed, or \
             the `.rs:<digits>` shape did - and every assertion below is vacuous.",
            citations.len()
        );

        // A path-qualified citation must resolve as a path; a bare filename may resolve by UNIQUE
        // basename. Matching a bare basename against a qualified path is how `src/options/mod.rs`
        // once appeared to resolve to `routes/mod.rs` - the candidate list is what makes the guard
        // sound, so it is spelled out rather than approximated.
        let mut by_basename: std::collections::HashMap<String, Vec<std::path::PathBuf>> =
            std::collections::HashMap::new();
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        for p in source_files() {
            if let Some(name) = p.file_name() {
                by_basename
                    .entry(name.to_string_lossy().into_owned())
                    .or_default()
                    .push(p.clone());
            }
        }

        let mut broken: Vec<String> = Vec::new();
        for c in &citations {
            // (3) a quoted example.
            if CITATION_EXAMPLES
                .iter()
                .any(|(f, t, n)| c.file == *f && c.target == *t && c.cited == *n)
            {
                continue;
            }

            let candidates: Vec<std::path::PathBuf> = if c.target.contains('/') {
                ["src/", ""].iter().fold(Vec::new(), |mut acc, pre| {
                    let p = root.join("src").join(format!("{pre}{}", c.target));
                    let p2 = root.join(format!("{pre}{}", c.target));
                    if p.is_file() {
                        acc.push(p);
                    }
                    if p2.is_file() {
                        acc.push(p2);
                    }
                    acc
                })
            } else {
                by_basename
                    .get(c.target.rsplit('/').next().unwrap_or(&c.target))
                    .filter(|v| v.len() == 1)
                    .cloned()
                    .unwrap_or_default()
            };

            if let Some(hit) = candidates.first() {
                if let Ok(target_text) = std::fs::read_to_string(hit) {
                    let target_text = target_text.replace("\r\n", "\n");
                    let target_lines: Vec<&str> = target_text.lines().collect();

                    // A RANGE'S END IS CHECKED TOO. It was not, and the hole was measurable: a
                    // planted range whose start was real code and whose end was far past the file
                    // passed, because only the start was examined. A span reaching past the end of
                    // its target is as wrong as a start on a blank line, and it is the shape a
                    // hurried edit produces - a range copied from one construct and stretched to
                    // cover another.
                    if let Some(end) = c.cited_end {
                        if end < c.cited {
                            broken.push(format!(
                                "{}:{} cites {}:{}-{} with the END before the START",
                                c.file, c.line, c.target, c.cited, end
                            ));
                            continue;
                        }
                        if end as usize > target_lines.len() {
                            broken.push(format!(
                                "{}:{} cites {}:{}-{} but {} has only {} lines",
                                c.file,
                                c.line,
                                c.target,
                                c.cited,
                                end,
                                c.target,
                                target_lines.len()
                            ));
                            continue;
                        }
                    }

                    let at = c.cited as usize;
                    let line = target_lines
                        .get(at.saturating_sub(1))
                        .map(|l| l.trim())
                        .unwrap_or("");
                    let punctuation_only = line.chars().all(|ch| ");}],".contains(ch));
                    if !line.is_empty() && !punctuation_only {
                        continue;
                    }
                    broken.push(format!(
                        "{}:{} cites {}:{} but that line is {}",
                        c.file,
                        c.line,
                        c.target,
                        c.cited,
                        if line.is_empty() {
                            "BLANK"
                        } else {
                            "punctuation only"
                        }
                    ));
                    continue;
                }
            }

            // (2) a dependency's own source.
            if DEPENDENCIES.iter().any(|d| c.block.contains(d)) {
                continue;
            }

            broken.push(format!(
                "{}:{} cites {}:{} which does not exist in this repository, and the comment names \
                 no dependency whose source it could be",
                c.file, c.line, c.target, c.cited
            ));
        }

        assert!(
            broken.is_empty(),
            "a line citation in a Rust comment points at nothing. A citation re-points at its \
             neighbour the moment a line is added above it, and the result READS as a plausible \
             reference - which is why these were only found by measuring. Name the SYMBOL instead \
             of the line: `clamp_debit` survives the file growing, `db.rs:266` does not. \
             Sites: {broken:#?}"
        );
    }

    /// `docs/benchmark.md` must not send a reader to a command that does something else.
    ///
    /// WHY THIS EXISTS. The section documented
    /// `cargo run --release --bin benchmark -- --concurrency 250 --duration 60s --scenarios
    /// streaming,ledger`, and promised a summary table of RPS, p50/p90/p95/p99 and RSS/CPU.
    /// MEASURED: that command RUNS TO COMPLETION - cargo passes the arguments and the binary
    /// ignores them - and prints the same four fixed scenarios it always prints. So the doc
    /// described a load test that does not exist, in the way that is hardest to notice: a command
    /// that succeeds, so nobody investigates.
    ///
    /// WHAT IS PINNED, and it is the narrow part that a reader would actually act on:
    ///   * the invocation has no flags, because the binary parses no arguments;
    ///   * the promised output - an RPS column and a percentile column - is named as NOT produced.
    ///
    /// WHAT IS NOT PINNED: that the scenarios still are the four this doc lists, or that their
    /// thresholds are the ones stated. Those need a parser for a println-driven binary, and a
    /// guard that re-states its subject in prose is the decoration `docs/testing.md` warns about.
    /// The struck claims below are the ones that were wrong; this test is what stops the flags
    /// coming back.
    #[test]
    fn the_benchmark_doc_describes_a_command_the_binary_actually_has() {
        let doc = std::fs::read_to_string(doc_path("benchmark.md"))
            .expect("docs/benchmark.md must be readable, or this passes over nothing");

        // THE BINARY PARSES NO ARGUMENTS. Assert that directly rather than trusting the prose: if
        // someone adds a parser, the invocation in the doc becomes legitimate and this test should
        // be the thing that says so.
        let bin = std::fs::read_to_string(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("src")
                .join("bin")
                .join("benchmark.rs"),
        )
        .expect("server/src/bin/benchmark.rs must be readable, or this passes over nothing");
        let parses_args = bin.contains("env::args")
            || bin.contains("args().collect")
            || bin.contains("clap::")
            || bin.contains("Parser::parse");
        assert!(
            !parses_args,
            "benchmark.rs now parses arguments, so the documented invocation may be real again. \
             Update docs/benchmark.md and this test together - do not just delete the assert."
        );

        // The invocation the doc shows must carry no flags, since the binary accepts none.
        let invocation = doc
            .lines()
            .find(|l| l.contains("--bin benchmark"))
            .unwrap_or_else(|| panic!("docs/benchmark.md no longer shows how to run the binary"));
        assert!(
            !invocation.contains(" -- "),
            "docs/benchmark.md shows `{invocation}` with arguments, and benchmark.rs parses none. \
             MEASURED: cargo passes them through and the binary ignores them, so the documented \
             command SUCCEEDS while running the fixed scenarios instead of the requested ones. \
             A reader who wants those flags wants a load test, which is a different tool."
        );

        // The struck claims must stay struck. An unstruck promise of a percentile table would
        // re-create the defect, because that output does not exist.
        //
        // SCOPED TO THE BINARY'S OWN SECTION, and the first version of this check was not: it
        // scanned the whole file and fired on `$p99 \le 2\text{ ms}$` in the TARGETS table at the
        // top, which is a real measured objective and nothing to do with this binary's output. A
        // guard that fails on a different section teaches people to ignore it - the same mistake
        // `the_published_retention_periods_...` records about bolding a table cell.
        let section = doc
            .split("## 4. Benchmark Tooling & Execution")
            .nth(1)
            .expect("docs/benchmark.md must still have its tooling section");
        for promised in ["Throughput (RPS)", "percentiles"] {
            let live = section.lines().any(|l| {
                let t = l.trim_start();
                l.contains(promised)
                    && !t.starts_with('>')
                    && !t.starts_with("~~")
                    && !t.contains("~~")
            });
            assert!(
                !live,
                "docs/benchmark.md's tooling section promises `{promised}` as benchmark output, \
                 and the binary produces no RPS table and no latency histogram. Real percentiles \
                 come from a load test against a running server, which is not what this binary is."
            );
        }
    }

    /// A comment must not claim data goes to **Postgres**. There is none.
    ///
    /// WHY THIS EXISTS. `hash_token`'s doc comment said the plaintext "never reaches Postgres", and
    /// `routes/mod.rs` had a second such line in a test. Both were present-tense claims about where
    /// a credential goes, and the port to SQLite is complete: `db.rs` is `SqlitePool` throughout and
    /// `main.rs` states outright that this deployment has no Postgres. A reader who believed the
    /// comment would reason about a network boundary that does not exist, and about the wrong
    /// transaction semantics and types behind it.
    ///
    /// The first one also cited `docs/website/02-data-model.md` as current authority - while
    /// `the_superseded_documents_say_so` in this same file already records that document as
    /// "SUPERSEDED - it documents the PostgreSQL schema the port replaced". The guard knew the
    /// citation was dead and the comment did not, which is the shape this file keeps meeting.
    ///
    /// WHAT IS ALLOWED, because most mentions here are GOOD and this check must not fight them: a
    /// PAST-TENSE note about the code that came before - "the Postgres original did this in one
    /// statement", "Ported from the Postgres original" - explains why a dialect choice was made and
    /// is cited to `docs/plans/sqlite-migration.md`. Those are history. The rule is about a claim in
    /// the present tense that the SYSTEM still stores somewhere that does not exist.
    ///
    /// SO THE CHECK IS NARROW, and it is narrow on purpose: it fires on a comment line that names
    /// Postgres AND uses a present-tense storage verb, and that does NOT carry one of the
    /// history markers. A broader check would flag the migration notes and be deleted by whoever
    /// touched the file next - the failure mode `strip_comments`' own doc comment describes.
    #[test]
    fn no_comment_claims_data_is_stored_in_a_database_this_crate_does_not_use() {
        // Present-tense storage verbs. A line must carry one of these AND name Postgres.
        const CLAIMS: &[&str] = &[
            "never reaches Postgres",
            "reaches Postgres",
            "stored in Postgres",
            "stored to Postgres",
            "goes to Postgres",
            "reaches PostgreSQL",
            "stored in PostgreSQL",
        ];
        // History markers. A line carrying one is a note about the old code, not a claim.
        const HISTORY: &[&str] = &[
            "original",
            "Ported",
            "ported",
            "used to",
            "migration",
            "no longer",
            "would have",
            "relied on",
            "the plan",
            "Postgres-shaped",
            "Postgres-only",
            "had Postgres",
            "Postgres defaults",
            "Postgres schema",
            "Postgres keeps",
            "Postgres stores",
            "Postgres stores",
            "requires live Postgres",
            "no Postgres is running",
            "a REAL Postgres",
            "Live Postgres",
            "if the constant",
        ];

        // THE POSITIVE CONTROL FOR THE LISTS THEMSELVES, which the two counters below cannot be.
        //
        // `files` and `comment_lines` prove the scan reached the crate's comments, but NEITHER
        // depends on `CLAIMS` - MEASURED: replacing the CLAIMS list with a string that cannot occur
        // left this test green even with both counters in place. The lists are the other half, and
        // their failure is silent in the opposite direction: a CLAIMS entry with a typo, or an empty
        // string, changes which lines are offenders without changing any count.
        for claim in CLAIMS {
            assert!(
                !claim.is_empty(),
                "CLAIMS must not carry an empty entry - `line.contains(\"\")` is true for EVERY \
                 line, so every comment would become an offender and the real ones would be lost in \
                 the flood."
            );
            assert!(
                claim.ends_with("Postgres") || claim.ends_with("PostgreSQL"),
                "the CLAIMS entry {claim:?} does not END with the database name, so it can never \
                 match the claim it was written to describe. Every entry here is a storage verb \
                 followed directly by `Postgres` or `PostgreSQL`, and the difference matters: a typo \
                 such as `Postgress` still CONTAINS `Postgres`, so a `contains` test accepts it and \
                 the guard runs, finds no offender, and reports success. MEASURED: mutating an entry \
                 to `stored in Postgress` passed a `contains`-based version of this assertion."
            );
            assert!(
                claim.len() > "PostgreSQL".len(),
                "the CLAIMS entry {claim:?} carries no storage verb in front of the database name. \
                 That would flag every comment mentioning Postgres - including the historical notes \
                 the HISTORY list exists to allow - and a guard that fires on correct code is deleted \
                 by whoever touches the file next."
            );
        }
        for marker in HISTORY {
            assert!(
                !marker.is_empty(),
                "HISTORY must not carry an empty marker - `line.contains(\"\")` is true for every \
                 line, so one empty entry would exempt the entire crate from this check."
            );
        }

        let mut offenders = Vec::new();
        // THE VACUITY GUARD THIS TEST WAS MISSING, and it is not a formality - every sibling scan in
        // this file has one and this was the outlier.
        //
        // The assertion at the end is `offenders.is_empty()`, which a scan that examines NOTHING
        // satisfies perfectly. MEASURED: replacing the CLAIMS list with a single string that cannot
        // occur in any line left this test GREEN (1 passed, 0 failed, and it COMPILED - the first
        // version of that mutation dropped a semicolon and failed to build, which is not a caught
        // mutation and proved nothing at all).
        //
        // The two counts close the SCOPE half of that hole: `files` proves `source_files()` returned
        // something, `comment_lines` proves the loop reached comment text. They do NOT close the
        // CLAIMS half - neither reads that list - which is why the loop just above exists.
        let mut files = 0usize;
        let mut comment_lines = 0usize;
        for path in source_files() {
            let text = std::fs::read_to_string(&path).unwrap_or_default();
            // `unwrap_or_default` means an UNREADABLE file yields an empty string and contributes
            // nothing - the same silent-skip shape. Counting only readable files makes that visible.
            if !text.is_empty() {
                files += 1;
            }
            for (n, line) in text.lines().enumerate() {
                let trimmed = line.trim_start();
                // comments only
                if !(trimmed.starts_with("//")
                    || trimmed.starts_with("///")
                    || trimmed.starts_with("//!"))
                {
                    continue;
                }
                comment_lines += 1;
                let claims_postgres = CLAIMS.iter().any(|c| line.contains(c));
                if !claims_postgres {
                    continue;
                }
                if HISTORY.iter().any(|h| line.contains(h)) {
                    continue;
                }
                // A QUOTED claim is a comment ABOUT the claim, not the claim. `hash_token`'s fixed
                // comment has to be able to quote the phrase it is correcting and explain why it was
                // wrong - a guard that forbids quoting makes the correction impossible to record,
                // which is how a guard gets deleted by the next person to touch the file.
                //
                // THE EXCLUSION IS "A QUOTE MARK ON EACH SIDE", not "the claim IS a quoted string".
                // MEASURED: the narrower form let a correction through that quoted a LONGER sentence
                // containing the claim, which is exactly how a correction reads. Matching before-and-
                // after is what makes that pass without also letting the bare claim through.
                //
                // THIS PARAGRAPH DOES NOT REPEAT THE PHRASE, deliberately: it did, and the guard fired
                // on its own comment. Reworded rather than special-cased, because a guard whose
                // exclusion depends on how a comment escapes its own quotes is a guard that will be
                // wrong again the next time somebody writes about it.
                let claims = CLAIMS
                    .iter()
                    .find(|c| line.contains(**c))
                    .copied()
                    .unwrap_or("");
                if let Some(at) = line.find(claims) {
                    let before = &line[..at];
                    let after = &line[at + claims.len()..];
                    let open = ['"', '`', '\u{201c}'];
                    let close = ['"', '`', '\u{201d}'];
                    if before.contains(open) && after.contains(close) {
                        continue;
                    }
                }
                offenders.push(format!("{}:{}: {}", path.display(), n + 1, line.trim()));
            }
        }

        // THE SCAN READ THE CRATE, before the assertion that it found nothing. `files` and
        // `comment_lines` are independent of the CLAIMS list, so a list that stopped matching is
        // caught here even though the offender check below would report success.
        assert!(
            files >= 25,
            "only {files} readable source file(s) were walked, so the Postgres-claim scan covered \
             almost nothing and its `offenders.is_empty()` result means nothing. A file that cannot \
             be read contributes no lines at all - that is the silent-skip shape rather than a \
             failure - so the count is over files whose text actually arrived."
        );
        assert!(
            comment_lines >= 1000,
            "only {comment_lines} comment line(s) were seen across {files} file(s). This crate's \
             comments run to 17,408 lines across 38 files at the time of writing, so a count this \
             low means the comment filter stopped matching - and a scan whose CLAIMS never meet a \
             comment passes while checking nothing. MEASURED: replacing CLAIMS with a string that \
             cannot occur left this test green before these counts existed."
        );

        assert!(
            offenders.is_empty(),
            "a comment claims data is stored in Postgres, and this crate has no Postgres - the port \
             to SQLite is complete and `main.rs` says so. A reader who believes this line reasons \
             about a network boundary that does not exist. If the line is a NOTE ABOUT THE OLD CODE, \
             say so in the past tense (\"the Postgres original ...\") and it will pass. Sites: \
             {offenders:#?}"
        );
    }
}
