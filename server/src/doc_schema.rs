//! The SCHEMA checked against the promises the customer-facing pages make about it.
//!
//! WHY A SEPARATE MODULE. The claims it guards live in website/src/lib/privacy.ts
//! and the enforcement side is the SQL in server/migrations, so the check has to
//! read a migration file from a test that otherwise has nothing to do with
//! documents. Putting it beside the other claim checks would have made that
//! module's name a lie.
//!
//! THE CLAIM. The privacy page says per-request usage carries token counts, cost
//! and time and never the prompt or the completion, and the page frames the whole
//! business on it: a proxy, not a data processor. It is the most load-bearing
//! sentence on a page a customer reads.
//!
//! VERIFIED TRUE before it was made a check, three ways: usage_events has no text
//! column of any kind; no migration mentions prompt, completion, request_body or
//! response_body; and the only free-text columns in the database are the three
//! review bodies, which are customer-written testimonials the same page discloses
//! separately.
//!
//! WHY A CHECK RATHER THAN A NOTE. Adding a prompt column for debugging is the most
//! ordinary way this claim would be broken, and it would be broken SILENTLY: the
//! column lands, every other test stays green, and the page goes on telling
//! customers their prompts are not stored. The one nearby test -
//! a_customer_prompt_never_reaches_the_log - covers the log, not the database.

/// The body of a CREATE TABLE statement, from its opening parenthesis to the one
/// that closes it.
///
/// Parenthesis depth, not a line scan, because a CHECK constraint in this schema
/// nests: reviews has CHECK (account_id IS NOT NULL OR telegram_id IS NOT NULL)
/// and stopping at the first close paren would truncate the table mid-definition
/// and quietly report fewer columns. No string-literal awareness, which is a real
/// limitation and a safe one here: a parenthesis inside a DEFAULT string would
/// need it, and no column in this schema has one.
fn table_body<'a>(sql: &'a str, table: &str) -> &'a str {
    let marker = format!("CREATE TABLE {table} ");
    let start = sql
        .find(&marker)
        .unwrap_or_else(|| panic!("{table} must exist in the schema, or this checks nothing"));
    let open = sql[start..]
        .find('(')
        .map(|at| start + at)
        .expect("a CREATE TABLE has an opening parenthesis");
    let mut depth = 0usize;
    for (offset, c) in sql[open..].char_indices() {
        match c {
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth == 0 {
                    return &sql[open..=open + offset];
                }
            }
            _ => {}
        }
    }
    panic!("{table} has no closing parenthesis; the schema is truncated");
}

/// The column names in a CREATE TABLE body, in declaration order.
///
/// A leading token that is a table CONSTRAINT rather than a column is skipped.
/// The list is read from the schema rather than hardcoded, so the assertion is
/// about the shape and not about a list kept in step with itself.
fn column_names(body: &str) -> Vec<String> {
    const CONSTRAINTS: &[&str] = &["CHECK", "CONSTRAINT", "PRIMARY", "FOREIGN", "UNIQUE"];
    let mut names = Vec::new();
    for line in body.lines() {
        let line = line.trim().trim_end_matches(',').trim();
        let Some(first) = line.split_whitespace().next() else {
            continue;
        };
        if !first
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        {
            continue;
        }
        if CONSTRAINTS.contains(&first) {
            continue;
        }
        names.push(first.to_string());
    }
    names
}

#[cfg(test)]
mod tests {
    use super::*;

    fn schema() -> String {
        std::fs::read_to_string(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("migrations")
                .join("20260925000000_initial_schema.sql"),
        )
        .expect("the initial schema must be readable, or this checks nothing")
    }

    #[test]
    fn the_request_path_tables_hold_no_free_text() {
        let sql = schema();
        assert_eq!(
            column_names(table_body(&sql, "usage_events")),
            vec![
                "id",
                "account_id",
                "api_key_id",
                "model",
                "input_tokens",
                "cache_read_tokens",
                "output_tokens",
                "cost_idr",
                "ref",
                "created_at",
            ],
            "usage_events no longer has exactly the columns listed here. A prompt, completion \
             or request-body column on this table would make the privacy page's central claim - \
             we are a proxy, not a data processor - false, and it would be false SILENTLY: \
             nothing else in the crate would notice. If the addition is deliberate, say so here \
             and update the page with it."
        );
    }

    /// The parser, on its own, because a parser that silently returns an empty list
    /// makes the assertion above pass over nothing and report a clean sheet about
    /// a table it never read.
    #[test]
    fn the_column_reader_survives_a_nested_check_constraint() {
        // reviews carries CHECK (a IS NOT NULL OR b IS NOT NULL) - the shape that
        // stops a naive scan at the first closing parenthesis and reports fewer
        // columns than the table has.
        let sql = schema();
        let names = column_names(table_body(&sql, "reviews"));
        assert_eq!(
            names.first().map(String::as_str),
            Some("id"),
            "the body must start at the column list, not at the CREATE TABLE line"
        );
        assert!(
            names.contains(&"withdrawn_at".to_string()),
            "a column declared AFTER the nested CHECK must still be found, got {names:?}"
        );
        assert!(
            !names.contains(&"CHECK".to_string()),
            "a constraint keyword is not a column, got {names:?}"
        );
    }
}
