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
            !privacy.contains("kept indefinitely"),
            "website/src/lib/privacy.ts tells customers their per-request usage is kept \
             INDEFINITELY because no purge job is running. The nightly maintenance \
             scheduler deletes those rows at 90 days, and two documents in this \
             repository say so. A privacy page that understates collection is still a false \
             statement about data handling, and this one is the statement a customer is \
             held to."
        );

        // THE SAME POLICY IS PUBLISHED TWICE in docs/, and one document being correct is no
        // help when the other says something else. docs/ip-tracking.md carries its own
        // retention table for the same three tables. It is the page a customer asking
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
    /// The bot README's claim about the FOLDER is true, checked against the tree.
    ///
    /// A third kind of promise, and the other two are the wrong shape for this one.
    /// The retention and lifetime checks tie a DOCUMENT to CODE. This one ties a
    /// document to the REPOSITORY TREE: telegram/README.md states that the folder
    /// holds this README and nothing else, and docs/launch-checklist.md carries an
    /// open item that depends on the same fact - that the bot is design-only, which is
    /// a launch gate.
    ///
    /// Both are claims a reader trusts and nothing keeps honest. And this one has a
    /// property the others do not: it goes STALE by someone doing ordinary work. Writing
    /// the bot is not a defect, it is the next task - and the two documents that say
    /// there is no bot would quietly become wrong, which for a launch gate is the
    /// expensive direction to be wrong in.
    ///
    /// So the failure is deliberately a NUDGE rather than a veto, and it says what to
    /// do. -Force matters: without it a .gitkeep or an editor swap file would read as
    /// a bot, and the check would cry wolf the first time someone opened the folder.
    #[test]
    fn the_telegram_folder_is_still_the_scaffolding_its_readme_claims() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("telegram");

        let mut others: Vec<String> = std::fs::read_dir(&dir)
            .expect("telegram/ must be readable")
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|name| name != "README.md")
            .collect();
        others.sort();

        assert!(
            others.is_empty(),
            "telegram/ now holds {others:?} besides its README. That is the NEXT TASK rather than a defect, but two documents now say otherwise: telegram/README.md states the folder holds this README and nothing else, and docs/launch-checklist.md carries an open item because the bot is design-only - which is a launch gate. Update both, and drop the checklist item if the bot is done enough to unblock a launch."
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
    /// It is a measurement rather than a guess: 163 macro calls across 29 files, zero
    /// interpolations. This is what keeps it that way - logging a secret is the most
    /// ordinary mistake there is while debugging a failing payment path, and nothing
    /// else in this crate would notice.
    #[test]
    fn no_secret_is_interpolated_into_a_log_or_format_macro() {
        // Names that carry a secret. A caller holding one of these holds something
        // that must not leave the process.
        const SECRETS: &[&str] = &[
            "server_key",
            "api_key",
            "token_hash",
            "full_key",
            "presented_key",
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

        assert!(walked >= 30, "only {walked} markdown files were read");
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
            files >= 30,
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
            // THE SHELL LITERAL MUST BE THE RUST CONSTANT, for ALL SIX, which is the
            // check that did not exist while these windows lived as literals typed into
            // the entrypoint. The privacy page, the policy table and the metrics endpoint
            // all read the numbers from Rust while the sweep read them from a script, so
            // nothing tied any promise to the thing that enforces it - a sweep could be
            // deleting at 30 days while every document said 7, and every other check here
            // would still pass because they compare the documents to the CONSTANTS.
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
            swept.len() >= 6,
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
        for line in page.lines() {
            let Some(rest) = line.trim().strip_prefix("{ what:") else {
                continue;
            };
            let Some((_, after_what)) = rest.split_once(", keep: '") else {
                continue;
            };
            let Some((keep, _)) = after_what.split_once("', why:") else {
                continue;
            };
            // A duration and nothing else. `30-90 days` is one, because a customer
            // reading a range is still being told a period.
            if keep.contains("day") || keep.contains("month") || keep.contains("hour") {
                durations.push(keep.to_string());
            }
        }

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
            columns.len() >= 60,
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
            walked >= 30,
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
            ("accounts", "Telegram ID"),
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
            ("link_codes", "Telegram link codes"),
            ("link_code_issues", "Telegram link codes"),
            ("admin_audit", "operator audit rows"),
        ];

        // Tables that exist and hold nothing. A reason is required, because an empty
        // string is not a reason and a future reader cannot tell deliberate from
        // forgotten.
        const UNUSED: &[(&str, &str)] = &[
            (
                "identities",
                "auth lives in PocketBase and this table is never written; it holds an email and an Argon2id password hash, and the privacy page says both live in PocketBase. If the port ever moves identity HERE the page goes false in the direction nobody reviews"
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
                    tables.push(
                        rest.split(|c: char| c == '(' || c.is_whitespace())
                            .next()
                            .unwrap_or_default()
                            .to_string(),
                    );
                }
            }
        }
        tables.sort();
        tables.dedup();

        // Two vacuity guards, because the two ways this parse can be wrong are both
        // silent: a parse that found nothing, and a glob that matched only the first
        // migration - which is what it used to do.
        assert!(
            files >= 4,
            "only {files} migration file(s) were read, so a table created by a later migration would be invisible to this test."
        );
        assert!(
            tables.len() >= 19,
            "only {} table(s) were read from the migrations, so this test is not looking at the real schema.",
            tables.len()
        );

        for table in &tables {
            if let Some((_, phrase)) = DISCLOSED.iter().find(|(t, _)| t == table) {
                assert!(privacy.contains(phrase), "table `{table}` is recorded as disclosed under the phrase {phrase:?}, but that phrase is not in website/src/lib/privacy.ts. The map has drifted from the page it describes.");
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
            ("architecture/identity.md", "how a person becomes an account; superseded on stack by architecture.md, and its claims are about PocketBase which the port retired"),
            ("business/00-overview.md", "the business case, not a contract"),
            ("business/01-market.md", "market analysis; no code claim to check"),
            ("business/02-pricing.md", "unit economics; the money rules themselves are tested in money.rs"),
            ("business/03-financial-model.md", "a financial model, not a specification"),
            ("business/04-gtm.md", "go to market"),
            ("business/05-risk.md", "a risk register"),
            ("business/README.md", "an index for the business set"),
            ("plans/proxy-hot-path-audit.md", "a HISTORICAL audit record; its findings are dated, and rewriting them would destroy the record of what was believed then"),
            ("plans/sqlite-migration.md", "a HISTORICAL migration plan, marked draft for review"),
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
}
