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
        let doc = std::fs::read_to_string(doc_path("data-retention.md"))
            .expect("docs/data-retention.md must be readable, or this passes over nothing");
        let ip_doc = std::fs::read_to_string(doc_path("ip-tracking.md"))
            .expect("docs/ip-tracking.md must be readable, or this passes over nothing");

        // Table named in the doc, the period the doc states, the constant enforcing it.
        let promises: [(&str, &str, i64); 5] = [
            (
                "usage_events",
                "**90 days**",
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
                "**7 days**",
                crate::ip_tracking::SEEN_RETENTION_DAYS,
            ),
            (
                "key_ip_daily",
                "**90 days**",
                crate::ip_tracking::DAILY_RETENTION_DAYS,
            ),
        ];

        for (table, stated, constant) in promises {
            assert!(
                doc.contains(stated),
                "docs/data-retention.md no longer states {stated} for {table}; a retention period nobody was told about is not a promise, and the constant is {constant} days. Either the row was reworded or the policy changed without the promise being updated."
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
    /// Every document is TRIAGED: either citation-checked, or listed here with a reason.
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
        const SWEEP: &[(&str, u32)] = &[
            ("key_ip_seen", 7),
            ("key_ip_daily", 90),
            ("usage_daily", 730),
            ("usage_events", 90),
            ("sessions", 30),
            ("link_redemption_attempts", 7),
        ];

        // Which document states each window. Not the same file throughout, which is
        // part of why this went unnoticed: the promise and the code were never in one
        // place to be compared.
        let sources = [
            read_repo_file("docs/data-retention.md"),
            read_repo_file("docs/ip-tracking.md"),
        ];
        let published = sources.join("\n");

        for (table, days) in SWEEP {
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
        let mut swept: Vec<String> = Vec::new();
        for line in entrypoint.lines() {
            if !line.contains("retention_delete") {
                continue;
            }
            let Some(rest) = line.split("$DB_FILE").nth(1) else {
                continue;
            };
            let name = rest
                .split_whitespace()
                .find(|t| !t.is_empty() && !t.contains('"'))
                .unwrap_or_default();
            if !name.is_empty() {
                swept.push(name.to_string());
            }
        }

        // The vacuity guard: a parse that found nothing would agree with anything.
        assert!(
            swept.len() >= 6,
            "only {} retention deletes were parsed from the entrypoint",
            swept.len()
        );

        for table in &swept {
            assert!(
                SWEEP.iter().any(|(t, _)| t == table),
                "the entrypoint deletes `{table}` but no window in this test claims to. Either the promise is missing from the documents, or this list is out of date - both are worth knowing, and neither may be resolved by silently editing the list."
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
    fn every_document_is_either_citation_checked_or_triaged_with_a_reason() {
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
