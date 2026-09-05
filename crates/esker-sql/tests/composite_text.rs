//! The composite text form, read and written — the half that needs no catalog.
//!
//! Every case is a row of `captures/pg19_composite.txt`. The round trip is the property that
//! matters: **a NULL field and an empty-string field are different values**, and the text form is
//! only lossless because it keeps them apart.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use esker_sql::value::composite;

#[test]
fn the_output_function_quotes_exactly_what_postgresql_quotes() {
    for (fields, want) in [
        (
            vec![Some("Paris"), Some("Champs-Élysées")],
            "(Paris,Champs-Élysées)",
        ),
        (vec![Some("a,b"), Some("c\"d")], "(\"a,b\",\"c\"\"d\")"),
        // A NULL is nothing at all and an empty string is `""` — the pair the format exists for.
        (vec![None, Some("x")], "(,x)"),
        (vec![Some(""), Some(" ")], "(\"\",\" \")"),
        (vec![Some("a(b)c"), Some("d\\e")], "(\"a(b)c\",\"d\\\\e\")"),
        // **Any** space quotes, including a leading or trailing one.
        (
            vec![Some("  lead"), Some("trail  ")],
            "(\"  lead\",\"trail  \")",
        ),
    ] {
        let owned: Vec<Option<String>> =
            fields.iter().map(|f| f.map(ToString::to_string)).collect();
        assert_eq!(composite::render(&owned), want, "{fields:?}");
    }
}

#[test]
fn the_input_function_reads_what_postgresql_reads() {
    for (text, want) in [
        ("(Paris,Rue Basse)", "(Paris,\"Rue Basse\")"),
        // Whitespace **inside** the parens is part of the value; outside it is not.
        ("( Paris , Rue Basse )", "(\" Paris \",\" Rue Basse \")"),
        (" (a,b) ", "(a,b)"),
        ("(\"a,b\",\"c\"\"d\")", "(\"a,b\",\"c\"\"d\")"),
        ("(,x)", "(,x)"),
        ("(\"\",x)", "(\"\",x)"),
        ("(\"a)b\",y)", "(\"a)b\",y)"),
        ("(a,)", "(a,)"),
        // **`NULL` written out is the four-character string**, not a NULL.
        ("(NULL,x)", "(NULL,x)"),
        // A backslash escapes inside quotes and outside them, and the output re-renders with a
        // doubled quote either way.
        ("(\"a\\\"b\",y)", "(\"a\"\"b\",y)"),
        ("(a\\,b,y)", "(\"a,b\",y)"),
    ] {
        assert_eq!(
            composite::canonicalise(text, 2).unwrap(),
            want,
            "reading {text:?}"
        );
    }
}

#[test]
fn a_null_field_and_an_empty_field_stay_different() {
    assert_eq!(composite::parse("(,x)").unwrap()[0], None);
    assert_eq!(
        composite::parse("(\"\",x)").unwrap()[0],
        Some(String::new()),
        "an empty *quoted* field is the empty string, which `.city IS NULL` answers false for"
    );
}

#[test]
fn every_malformed_literal_is_one_message() {
    for text in ["()", "(a,b,c)", "(a", "a,b"] {
        let error = composite::canonicalise(text, 2)
            .expect_err("a malformed record literal must be refused");
        assert_eq!(error.sqlstate(), "22P02", "{text}: {error}");
        assert_eq!(
            error.to_string(),
            format!("malformed record literal: \"{text}\""),
            "PostgreSQL quotes the literal back, whatever the fault"
        );
    }
}
