//! What a body reads as — the census's own bodies, one test per construct — and what PostgreSQL 19
//! answered for every body it refuses, captured 2026-09-13 in one rolled-back session.

use super::lex::{Kind, tokenize};
use super::{Block, Context, Declaration, RaiseLevel, Statement, Target, VariableType, parse};

fn read(body: &str, context: Context) -> Block {
    parse(body, context).unwrap_or_else(|error| panic!("{body}\n  did not read: {error}"))
}

fn answer(body: &str, context: Context) -> String {
    match parse(body, context) {
        Ok(block) => panic!("{body}\n  read, as {block:?}"),
        Err(error) => format!("{} {error}", error.sqlstate()),
    }
}

fn sql(text: &str) -> Statement {
    Statement::Sql {
        text: text.to_owned(),
    }
}

// ----------------------------------------------------------------------------------------------
// The tokenizer
// ----------------------------------------------------------------------------------------------

/// **A `;` ends a statement only outside a string, an identifier, a dollar quote and a comment**,
/// and every one of those is a place a suite body or a user puts one.
#[test]
fn a_semicolon_inside_a_quote_or_a_comment_ends_nothing() {
    let body = "'a;b' \"c;d\" $q$ e;f $q$ E'g\\';h' -- i;j\n /* k; /* l; */ m; */ ;";
    let tokens = tokenize(body).unwrap();
    let kinds: Vec<Kind> = tokens.iter().map(|token| token.kind).collect();
    assert_eq!(
        kinds,
        [
            Kind::Literal,
            Kind::QuotedIdent,
            Kind::Literal,
            Kind::Literal,
            Kind::Punct
        ]
    );
    assert_eq!(tokens[0].literal(body).unwrap(), "a;b");
    assert_eq!(tokens[1].identifier(body).unwrap(), "c;d");
    assert_eq!(tokens[2].literal(body).unwrap(), " e;f ");
    assert_eq!(tokens[3].literal(body).unwrap(), "g';h");
}

/// **`n=-1` is `n`, `=`, `-`, `1`**: PostgreSQL's operator does not end in a sign unless it holds
/// one of `~`, `!`, `@`, `#`, `%`, `^`, `&`, `|`, a backtick or `?` — which is what lets an
/// assignment be written without spaces.
#[test]
fn an_operator_does_not_end_in_a_sign() {
    let body = "n=-1 a<=-b c@-d e:=f g::h";
    let texts: Vec<&str> = tokenize(body)
        .unwrap()
        .iter()
        .map(|token| token.text(body))
        .collect();
    assert_eq!(
        texts,
        [
            "n", "=", "-", "1", "a", "<=", "-", "b", "c", "@-", "d", "e", ":=", "f", "g", "::", "h"
        ]
    );
}

/// `1..3` is a number, an operator and a number — the shape of an integer `FOR`, which the subset
/// refuses by name and so has to see.
#[test]
fn a_range_is_not_a_fraction() {
    let body = "1..3 1.5 .5 2e10";
    let tokens = tokenize(body).unwrap();
    let texts: Vec<(Kind, &str)> = tokens
        .iter()
        .map(|token| (token.kind, token.text(body)))
        .collect();
    assert_eq!(
        texts,
        [
            (Kind::Number, "1"),
            (Kind::Operator, ".."),
            (Kind::Number, "3"),
            (Kind::Number, "1.5"),
            (Kind::Number, ".5"),
            (Kind::Number, "2e10"),
        ]
    );
}

/// A quoted identifier keeps its case and reads `""` as one quote; a word folds.
#[test]
fn an_identifier_folds_unless_it_is_quoted() {
    let body = "MaxValue \"MaxValue\" \"a\"\"b\"";
    let names: Vec<String> = tokenize(body)
        .unwrap()
        .iter()
        .map(|token| token.identifier(body).unwrap())
        .collect();
    assert_eq!(names, ["maxvalue", "MaxValue", "a\"b"]);
}

// ----------------------------------------------------------------------------------------------
// The census's bodies, verbatim
// ----------------------------------------------------------------------------------------------

/// **D1**: `create_enum`'s block, as `postgresql_adapter.rb:556` builds it.
#[test]
fn the_create_enum_block_reads() {
    let body = "\nBEGIN\n    IF NOT EXISTS (\n      SELECT 1\n      FROM pg_type t\n      JOIN \
                pg_namespace n ON t.typnamespace = n.oid\n      WHERE t.typname = 'mood'\n        \
                AND n.nspname = 'public'\n    ) THEN\n        CREATE TYPE \"mood\" AS ENUM ('sad', \
                'ok');\n    END IF;\nEND\n";
    let block = read(body, Context::Do);
    assert!(block.declarations.is_empty());
    let [Statement::If { condition, then }] = block.statements.as_slice() else {
        panic!("{block:?}");
    };
    assert!(condition.starts_with("NOT EXISTS (") && condition.ends_with(')'));
    assert_eq!(
        then.as_slice(),
        [sql("CREATE TYPE \"mood\" AS ENUM ('sad', 'ok')")]
    );
}

/// **D2**: `postgresql_adapter_test.rb`'s warning, lowercase `do` and all.
#[test]
fn the_warning_block_reads() {
    assert_eq!(
        read(
            " BEGIN RAISE WARNING 'PostgreSQL SQL warning'; END; ",
            Context::Do
        )
        .statements,
        [Statement::Raise {
            level: RaiseLevel::Warning,
            message: "PostgreSQL SQL warning".to_owned()
        }]
    );
}

/// **D3**: `check_all_foreign_keys_valid!`, as `referential_integrity.rb:41` writes it — with a
/// `%1$I` inside a string that must not read as a dollar quote.
#[test]
fn check_all_foreign_keys_valid_reads() {
    let body = r"
  declare r record;
BEGIN
FOR r IN (
  SELECT FORMAT(
    'UPDATE pg_catalog.pg_constraint SET convalidated=false WHERE conname = ''%1$I'' AND connamespace::regnamespace = ''%2$I''::regnamespace; ALTER TABLE %2$I.%3$I VALIDATE CONSTRAINT %1$I;',
    constraint_name,
    table_schema,
    table_name
  ) AS constraint_check
  FROM information_schema.table_constraints WHERE constraint_type = 'FOREIGN KEY'
)
  LOOP
    EXECUTE (r.constraint_check);
  END LOOP;
END;
";
    let block = read(body, Context::Do);
    assert_eq!(
        block.declarations,
        [Declaration {
            name: "r".to_owned(),
            ty: VariableType::Record
        }]
    );
    let [
        Statement::ForQuery {
            record,
            query,
            body,
        },
    ] = block.statements.as_slice()
    else {
        panic!("{block:?}");
    };
    assert_eq!(record, "r");
    assert!(query.starts_with("(\n  SELECT FORMAT(") && query.ends_with("'FOREIGN KEY'\n)"));
    assert_eq!(
        body.as_slice(),
        [Statement::Execute {
            command: "(r.constraint_check)".to_owned()
        }]
    );
}

/// **F2**: `populate_column`, statement 790's trigger function.
#[test]
fn populate_column_reads() {
    let body = "\nDECLARE\n  max_value INTEGER;\nBEGIN\n    SELECT MAX(id) INTO max_value FROM \
                pk_autopopulated_by_a_trigger_records;\n    NEW.id = COALESCE(max_value, 0) + \
                1;\n    RETURN NEW;\nEND;\n";
    let block = read(body, Context::Trigger);
    assert_eq!(
        block.declarations,
        [Declaration {
            name: "max_value".to_owned(),
            ty: VariableType::Sql("INTEGER".to_owned())
        }]
    );
    assert_eq!(
        block.statements,
        [
            Statement::SelectInto {
                query: "SELECT MAX(id) FROM pk_autopopulated_by_a_trigger_records".to_owned(),
                target: Target::Variable("max_value".to_owned()),
            },
            Statement::Assign {
                target: Target::Field {
                    record: "new".to_owned(),
                    field: "id".to_owned()
                },
                expression: "COALESCE(max_value, 0) + 1".to_owned(),
            },
            Statement::Return {
                expression: Some("NEW".to_owned())
            },
        ]
    );
}

/// **F1**: `partitioned_insert_trigger`, statement 762's — `INSERT INTO` spends its `INTO` on the
/// table, so this is an SQL statement and not a `SELECT … INTO`.
#[test]
fn partitioned_insert_trigger_reads() {
    let body = "\n    BEGIN\n      INSERT INTO postgresql_partitioned_table VALUES (NEW.*);\n      \
                RETURN NULL;\n    END;\n    ";
    assert_eq!(
        read(body, Context::Trigger).statements,
        [
            sql("INSERT INTO postgresql_partitioned_table VALUES (NEW.*)"),
            Statement::Return {
                expression: Some("NULL".to_owned())
            },
        ]
    );
}

// ----------------------------------------------------------------------------------------------
// One construct at a time
// ----------------------------------------------------------------------------------------------

#[test]
fn null_reads() {
    assert_eq!(
        read("BEGIN NULL; END", Context::Do).statements,
        [Statement::Null]
    );
}

/// `=` and `:=` are one assignment, and the value runs to the `;` whatever operators it holds.
#[test]
fn an_assignment_reads_with_either_operator() {
    let block = read(
        "DECLARE n integer; b boolean; BEGIN n = 5; n := -1; n=-2; b := (n = 1); END",
        Context::Do,
    );
    let values: Vec<&str> = block
        .statements
        .iter()
        .map(|statement| match statement {
            Statement::Assign {
                target: Target::Variable(_),
                expression,
            } => expression.as_str(),
            other => panic!("{other:?}"),
        })
        .collect();
    assert_eq!(values, ["5", "-1", "-2", "(n = 1)"]);
}

/// A quoted name is a different variable from its folded spelling.
#[test]
fn a_quoted_variable_keeps_its_case() {
    let block = read(
        "DECLARE \"Mixed\" integer; BEGIN \"Mixed\" = 1; END",
        Context::Do,
    );
    assert_eq!(block.declarations[0].name, "Mixed");
    assert!(matches!(
        &block.statements[0],
        Statement::Assign { target: Target::Variable(name), .. } if name == "Mixed"
    ));
}

/// Multi-word type names and typmods read as one type; the text is kept as written.
#[test]
fn a_declared_type_is_kept_as_written() {
    let block = read(
        "DECLARE a double precision; b character varying(10); c timestamp(3) with time zone; d \
         numeric(10, 2)[]; e public.mood; BEGIN NULL; END",
        Context::Do,
    );
    let types: Vec<&VariableType> = block.declarations.iter().map(|d| &d.ty).collect();
    assert_eq!(
        types,
        [
            &VariableType::Sql("double precision".to_owned()),
            &VariableType::Sql("character varying(10)".to_owned()),
            &VariableType::Sql("timestamp(3) with time zone".to_owned()),
            &VariableType::Sql("numeric(10, 2)[]".to_owned()),
            &VariableType::Sql("public.mood".to_owned()),
        ]
    );
}

#[test]
fn an_if_reads_and_nests() {
    let block = read(
        "BEGIN IF a THEN IF (b) THEN NULL; END IF; END IF; END",
        Context::Do,
    );
    assert_eq!(
        block.statements,
        [Statement::If {
            condition: "a".to_owned(),
            then: vec![Statement::If {
                condition: "(b)".to_owned(),
                then: vec![Statement::Null]
            }]
        }]
    );
}

/// No level is `EXCEPTION`; `%%` is `%`; an `E''` string reads its escapes; `RAISE;` re-raises.
#[test]
fn raise_reads_its_level_and_its_literal() {
    let block = read(
        "BEGIN RAISE NOTICE 'it''s'; RAISE 'no level'; RAISE EXCEPTION '100%%'; RAISE WARNING \
         E'a\\tb'; RAISE; END",
        Context::Do,
    );
    assert_eq!(
        block.statements,
        [
            Statement::Raise {
                level: RaiseLevel::Notice,
                message: "it's".to_owned()
            },
            Statement::Raise {
                level: RaiseLevel::Exception,
                message: "no level".to_owned()
            },
            Statement::Raise {
                level: RaiseLevel::Exception,
                message: "100%".to_owned()
            },
            Statement::Raise {
                level: RaiseLevel::Warning,
                message: "a\tb".to_owned()
            },
            Statement::Reraise,
        ]
    );
}

/// The `INTO` clause is cut out wherever it stands at depth zero, and an `INTO` inside a subquery
/// is the subquery's.
#[test]
fn select_into_cuts_its_into_clause() {
    let block = read(
        "DECLARE n integer; BEGIN SELECT id INTO n FROM t WHERE name = (SELECT x FROM u); SELECT \
         count(*) FROM t INTO n; END",
        Context::Do,
    );
    assert_eq!(
        block.statements,
        [
            Statement::SelectInto {
                query: "SELECT id FROM t WHERE name = (SELECT x FROM u)".to_owned(),
                target: Target::Variable("n".to_owned()),
            },
            Statement::SelectInto {
                query: "SELECT count(*) FROM t".to_owned(),
                target: Target::Variable("n".to_owned()),
            },
        ]
    );
}

#[test]
fn a_for_over_a_query_reads_with_or_without_brackets() {
    let block = read(
        "DECLARE r record; BEGIN FOR r IN SELECT 1 LOOP NULL; END LOOP; FOR r IN (SELECT 2) LOOP \
         UPDATE t SET a = r.a; END LOOP; END",
        Context::Do,
    );
    assert_eq!(
        block.statements,
        [
            Statement::ForQuery {
                record: "r".to_owned(),
                query: "SELECT 1".to_owned(),
                body: vec![Statement::Null],
            },
            Statement::ForQuery {
                record: "r".to_owned(),
                query: "(SELECT 2)".to_owned(),
                body: vec![sql("UPDATE t SET a = r.a")],
            },
        ]
    );
}

/// The command is an expression: a string, a concatenation, a bracketed field — and a dollar quote
/// holding two statements is still one expression.
#[test]
fn execute_reads_an_expression() {
    let block = read(
        "DECLARE tbl text; BEGIN EXECUTE 'DELETE FROM ' || tbl; EXECUTE $q$ SELECT 1; SELECT 2 \
         $q$; END",
        Context::Do,
    );
    assert_eq!(
        block.statements,
        [
            Statement::Execute {
                command: "'DELETE FROM ' || tbl".to_owned()
            },
            Statement::Execute {
                command: "$q$ SELECT 1; SELECT 2 $q$".to_owned()
            },
        ]
    );
}

/// A bare `RETURN;` ends a `DO` block.
#[test]
fn a_bare_return_reads_in_a_do_block() {
    assert_eq!(
        read("BEGIN RETURN; END", Context::Do).statements,
        [Statement::Return { expression: None }]
    );
}

/// An SQL statement runs to its `;` at depth zero, whatever it holds.
#[test]
fn any_other_statement_is_sql() {
    let block = read(
        "BEGIN CREATE TABLE t (a int, b text DEFAULT ';'); /* ; */ DELETE FROM t; END",
        Context::Do,
    );
    assert_eq!(
        block.statements,
        [
            sql("CREATE TABLE t (a int, b text DEFAULT ';')"),
            sql("DELETE FROM t")
        ]
    );
}

/// A declared `found` is an ordinary variable; only the implicit one is refused.
#[test]
fn a_declared_found_is_a_variable() {
    read(
        "DECLARE found integer; BEGIN IF found = 1 THEN NULL; END IF; END",
        Context::Do,
    );
}

// ----------------------------------------------------------------------------------------------
// PostgreSQL's answers for a body it refuses
// ----------------------------------------------------------------------------------------------

/// **Every body PostgreSQL 19 refused, with its code and its sentence**, from
/// `pg19_plpgsql_do.txt` and the capture of the parse errors beside it.
const MALFORMED: &[(&str, Context, &str)] = &[
    (
        " BEGIN IF true NULL; END IF; END ",
        Context::Do,
        "42601 missing \"THEN\" at end of SQL expression",
    ),
    (
        " BEGIN IF true ",
        Context::Do,
        "42601 syntax error at end of input",
    ),
    (
        " DECLARE n integer; BEGIN n = 1 ",
        Context::Do,
        "42601 syntax error at end of input",
    ),
    (
        " DECLARE r record; BEGIN FOR r IN SELECT 1 NULL; END LOOP; END ",
        Context::Do,
        "42601 missing \"LOOP\" at end of SQL expression",
    ),
    (
        " BEGIN RAISE WARNING 'a', 'b'; END ",
        Context::Do,
        "42601 too many parameters specified for RAISE",
    ),
    (
        " BEGIN RAISE NOTICE '100%'; END ",
        Context::Do,
        "42601 too few parameters specified for RAISE",
    ),
    (
        " BEGIN RAISE NOTICE '%%%'; END ",
        Context::Do,
        "42601 too few parameters specified for RAISE",
    ),
    (
        " DECLARE n integer ",
        Context::Do,
        "42601 incomplete data type declaration at end of input",
    ),
    (
        " BEGIN RAISE NOTICE 'unterminated; END ",
        Context::Do,
        "42601 unterminated quoted string at or near \"'unterminated; END \"",
    ),
    (
        " DECLARE n integer; n integer; BEGIN NULL; END ",
        Context::Do,
        "42601 duplicate declaration at or near \"n\"",
    ),
    (
        " BEGIN NULL; END lbl ",
        Context::Do,
        "42601 end label \"lbl\" specified for unlabeled block",
    ),
    (
        " BEGIN ; END ",
        Context::Do,
        "42601 syntax error at or near \";\"",
    ),
    (
        " BEGIN FOR n IN SELECT 1 LOOP NULL; END LOOP; END ",
        Context::Do,
        "42601 loop variable of loop over rows must be a record variable or list of scalar \
             variables",
    ),
    (
        " BEGIN EXECUTE ; END ",
        Context::Do,
        "42601 missing expression at or near \";\"",
    ),
    (
        " BEGIN IF THEN NULL; END IF; END ",
        Context::Do,
        "42601 missing expression at or near \"THEN\"",
    ),
    (
        " BEGIN NULL; ",
        Context::Do,
        "42601 syntax error at end of input",
    ),
    (
        " NULL; ",
        Context::Do,
        "42601 syntax error at or near \"NULL\"",
    ),
    (
        " BEGIN x.y = 1; END ",
        Context::Do,
        "42601 \"x.y\" is not a known variable",
    ),
    (
        " BEGIN INSERT INTO nosuch_t VALUES (1) ",
        Context::Do,
        "42601 unexpected end of function definition at end of input",
    ),
    (
        " BEGIN EXECUTE 'SELECT 1' INTO STRICT; END ",
        Context::Do,
        "42601 syntax error at or near \";\"",
    ),
    (
        " BEGIN IF true THEN NULL; END ",
        Context::Do,
        "42601 syntax error at end of input",
    ),
    (
        " BEGIN NULL END ",
        Context::Do,
        "42601 syntax error at or near \"END\"",
    ),
    (
        " DECLARE n integer BEGIN NULL; END ",
        Context::Do,
        "42601 syntax error at or near \"BEGIN\"",
    ),
    (
        " BEGIN SELECT 1 INTO nosuch; END ",
        Context::Do,
        "42601 \"nosuch\" is not a known variable",
    ),
    (
        " BEGIN nosuch = 1; END ",
        Context::Do,
        "42601 \"nosuch\" is not a known variable",
    ),
    (
        " BEGIN SELECT 1 END ",
        Context::Do,
        "42601 unexpected end of function definition at end of input",
    ),
    (
        " BEGIN RETURN 1; END ",
        Context::Do,
        "42804 RETURN cannot have a parameter in function returning void",
    ),
    (
        " BEGIN NEW.id = 1; END ",
        Context::Do,
        "42601 \"new.id\" is not a known variable",
    ),
    (
        " BEGIN RETURN; END ",
        Context::Trigger,
        "42601 missing expression at or near \";\"",
    ),
];

#[test]
fn a_malformed_body_answers_postgresqls_sentence() {
    for (body, context, expected) in MALFORMED {
        assert_eq!(answer(body, *context), *expected, "for {body}");
    }
}

/// **Every construct PostgreSQL runs and the subset does not is `0A000` naming it** — the rows
/// PostgreSQL answered `DO` or `CREATE FUNCTION` for, each one a declared divergence.
const OUTSIDE_THE_SUBSET: &[(&str, Context, &str)] = &[
    (
        "DECLARE n integer := 1; BEGIN NULL; END",
        Context::Do,
        "a default value in a declaration",
    ),
    (
        "BEGIN IF true THEN NULL; ELSE NULL; END IF; END",
        Context::Do,
        "ELSE",
    ),
    (
        "BEGIN IF true THEN NULL; ELSIF false THEN NULL; END IF; END",
        Context::Do,
        "ELSIF",
    ),
    ("BEGIN PERFORM 1; END", Context::Do, "PERFORM"),
    (
        "BEGIN WHILE false LOOP NULL; END LOOP; END",
        Context::Do,
        "WHILE",
    ),
    ("BEGIN BEGIN NULL; END; END", Context::Do, "nested blocks"),
    (
        "BEGIN NULL; EXCEPTION WHEN others THEN NULL; END",
        Context::Do,
        "EXCEPTION",
    ),
    (
        "BEGIN RAISE WARNING 'a %', 'b'; END",
        Context::Do,
        "RAISE with format arguments",
    ),
    (
        "DECLARE n integer; BEGIN FOR n IN SELECT 1 LOOP NULL; END LOOP; END",
        Context::Do,
        "FOR over a query into a scalar variable",
    ),
    (
        "BEGIN FOR i IN 1..3 LOOP NULL; END LOOP; END",
        Context::Do,
        "integer FOR loops",
    ),
    (
        "DECLARE r record; BEGIN r = NULL; END",
        Context::Do,
        "assignment to a whole record",
    ),
    (
        "BEGIN RAISE WARNING 'x' USING HINT = 'y'; END",
        Context::Do,
        "RAISE ... USING",
    ),
    ("BEGIN RAISE INFO 'i'; END", Context::Do, "RAISE INFO"),
    ("BEGIN RAISE LOG 'l'; END", Context::Do, "RAISE LOG"),
    (
        "BEGIN RAISE division_by_zero; END",
        Context::Do,
        "RAISE of a condition name",
    ),
    (
        "BEGIN IF found THEN NULL; END IF; END",
        Context::Do,
        "FOUND",
    ),
    (
        "DECLARE n integer; m integer; BEGIN SELECT 1, 2 INTO n, m; END",
        Context::Do,
        "SELECT ... INTO more than one target",
    ),
    (
        "DECLARE r record; BEGIN SELECT 1 AS a INTO r; END",
        Context::Do,
        "SELECT ... INTO a record",
    ),
    (
        "DECLARE n integer; BEGIN INSERT INTO t VALUES (1) RETURNING 1 INTO n; END",
        Context::Do,
        "INSERT ... INTO",
    ),
    (
        "BEGIN RAISE EXCEPTION 'x' USING ERRCODE = 'P0002'; END",
        Context::Do,
        "RAISE ... USING",
    ),
    ("<<outer>> BEGIN NULL; END", Context::Do, "block labels"),
    (
        "BEGIN EXECUTE 'SELECT 1' INTO n; END",
        Context::Do,
        "EXECUTE ... INTO",
    ),
    (
        "BEGIN EXECUTE 'SELECT $1' USING 1; END",
        Context::Do,
        "EXECUTE ... USING",
    ),
    ("BEGIN RETURN NEXT 1; END", Context::Do, "RETURN NEXT"),
    (
        "BEGIN GET DIAGNOSTICS n = ROW_COUNT; END",
        Context::Do,
        "GET DIAGNOSTICS",
    ),
    (
        "DECLARE c CURSOR FOR SELECT 1; BEGIN NULL; END",
        Context::Do,
        "cursor declarations",
    ),
    (
        "DECLARE n CONSTANT integer := 1; BEGIN NULL; END",
        Context::Do,
        "CONSTANT",
    ),
    (
        "DECLARE n t.col%TYPE; BEGIN NULL; END",
        Context::Do,
        "%TYPE",
    ),
    ("BEGIN COMMIT; END", Context::Do, "COMMIT"),
    (
        "BEGIN IF TG_OP = 'INSERT' THEN NULL; END IF; RETURN NEW; END",
        Context::Trigger,
        "TG_OP",
    ),
];

#[test]
fn a_construct_outside_the_subset_is_refused_by_name() {
    for (body, context, construct) in OUTSIDE_THE_SUBSET {
        assert_eq!(
            answer(body, *context),
            format!("0A000 PL/pgSQL {construct} is not supported"),
            "for {body}"
        );
    }
}
