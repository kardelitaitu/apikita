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
