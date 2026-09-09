//! `inet`, `cidr` and `macaddr`, against PostgreSQL 19beta1.
//!
//! Run 59's tier-3 row: 8 tests in `adapters/postgresql/network_test.rb`, which declares all three
//! in one `create_table` with a default each — so any one missing fails all 8.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "parity_harness/mod.rs"]
mod parity;

/// Nothing: the corpus builds its own table.
const CORPUS_FIXTURE: &[&str] = &[];

/// What this node answers differently, and why.
const DIVERGENCES: parity::Divergences = parity::Divergences {
    // The standing catalog type trade, five times: `typname` and `udt_name` are `name`, the two
    // oids are `oid`, `typcategory` is a `"char"` and `typinput` a `regproc`,
    // `information_schema`'s columns are its own domains, `pg_typeof` answers a `regtype`, and a
    // `::varchar` cast is a `character varying` — all `text` here. **Every value agrees**, and
    // the values are what these five ask: `inet` is 869 with array 1041, `cidr` 650/651 and
    // `macaddr` 829/1040; the two addresses are category `I` and the `macaddr` `U`; `macaddr`'s
    // `typlen` is 6 where the other two are varlenas; the three defaults read back
    // `'192.168.1.1'::inet`, `'192.168.1.0/24'::cidr` and `'ff:ff:ff:ff:ff:ff'::macaddr`, which
    // is what the schema dumper parses.
    types: &[
        "SELECT 'r', typname, oid, typarray, typlen, typcategory, typinput FROM pg_type WHERE \
         typname IN ('inet','cidr','macaddr','_inet','_cidr','_macaddr') ORDER BY typname",
        "SELECT 'r', column_name, data_type, udt_name, column_default FROM \
         information_schema.columns WHERE table_name = 'n' ORDER BY ordinal_position",
        "SELECT 'r', pg_typeof('192.168.1.1'::inet), pg_typeof('192.168.1.0/24'::cidr), \
         pg_typeof('ff:ff:ff:ff:ff:ff'::macaddr)",
        "SELECT 'r', '{192.168.1.1,10.0.0.1}'::inet[], pg_typeof('{192.168.1.1}'::inet[])",
    ],
    answers: &[
        // **The network functions and operators are a unit of their own, and none of them is in
        // the suite.** `network_test.rb` stores addresses and reads them back; `host`, `masklen`,
        // `network`, `broadcast`, `abbrev`, `family` and `text` are what a query *about* an
        // address uses, and `<<` / `>>` / `<<=` are the containment family that goes with them.
        // Named rather than answered, which is what a `0A000` is for.
        (
            "SELECT 'r', host('192.168.1.5/24'::inet), masklen('192.168.1.5/24'::inet), \
             network('192.168.1.5/24'::inet), broadcast('192.168.1.5/24'::inet)",
            "the network functions are their own unit",
            "UNMEASURED",
        ),
        (
            "SELECT 'r', abbrev('192.168.1.0/24'::cidr), family('192.168.1.1'::inet), \
             family('::1'::inet), text('192.168.1.5/24'::inet)",
            "the network functions are their own unit",
            "UNMEASURED",
        ),
        (
            "SELECT 'r', '192.168.1.5'::inet << '192.168.1.0/24'::inet, '192.168.1.0/24'::inet \
             >> '192.168.1.5'::inet, '192.168.1.5'::inet <<= '192.168.1.5/32'::inet",
            "the containment operators are their own unit",
            "UNMEASURED",
        ),
        // **Address arithmetic**, which is the same unit seen from the other side: `inet + 1` is
        // the next address and `inet - inet` is a `bigint` count of them. The refusal is the
        // ordinary "no such operator" rather than a `0A000`, because `+` and `-` exist and this
        // pair of operand types does not.
        (
            "SELECT 'r', '192.168.1.1'::inet + 1, '192.168.1.2'::inet - '192.168.1.1'::inet",
            "address arithmetic is part of the network-operator unit",
            "UNMEASURED",
        ),
    ],
};

#[test]
fn every_network_answer_is_postgresql_19_s() {
    let checked = parity::replay(
        include_str!("corpus/pg19_inet.txt"),
        CORPUS_FIXTURE,
        &DIVERGENCES,
    );
    assert!(
        checked > 30,
        "only {checked} statements ran; the corpus did not load"
    );
}
