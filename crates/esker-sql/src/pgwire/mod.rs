//! The PostgreSQL wire protocol, version 3, written here rather than taken from a crate.
//!
//! [`message`] is the whole protocol as pure functions over bytes. This module adds the two
//! decisions that sit just above it: how a [`SqlError`] becomes an `ErrorResponse` a client can
//! branch on, and what to answer a client that asks for a protocol version we do not speak.
//!
//! # The negotiation that decides whether modern clients can connect at all
//!
//! A client's startup packet carries a minor version. `libpq` has spoken 3.0 for twenty years, but
//! it now knows 3.2 and asks for it when told to — measured here, `psql` 18.6 sends 3.0 by default
//! and 3.2 with `max_protocol_version=latest` — and the default is expected to move.
//!
//! The protocol's answer to "I do not have that version" is [`Backend::NegotiateProtocolVersion`],
//! which says what the newest supported version is and is then **followed by the ordinary startup
//! sequence**. It is a downgrade notice, not a refusal. A server that instead answers an error is
//! simply unreachable by such a client, which is a failure with no diagnostic on either side.
//! `tests/golden/pgwire.hex` holds a real one, captured from PostgreSQL 19beta1 by asking it for
//! minor version 9.
//!
//! The same message reports `_pq_.`-prefixed protocol options the client asked for and the server
//! does not know; PostgreSQL 19 lists them by name, and so does [`negotiation`].

pub mod message;
pub mod server;
pub mod session;

use crate::error::{Severity, SqlError};
use message::{Backend, ErrorField, PROTOCOL_MAJOR, PROTOCOL_MINOR, Startup};

/// Builds the `ErrorResponse` field list for an error.
///
/// The order is PostgreSQL's own, verified against a captured `ErrorResponse` from a real server:
/// `S`, `V`, `C`, `M`, then the optional fields. Severity appears twice on purpose — `S` is
/// localised and `V` is not, and a client that branches on severity is supposed to read `V`, which
/// is why sending only one of them breaks drivers in a way that is hard to see from here.
#[must_use]
pub fn error_fields(error: &SqlError) -> Vec<(ErrorField, String)> {
    let severity = error.severity().as_str().to_owned();
    let mut fields = vec![
        (ErrorField::SEVERITY, severity.clone()),
        (ErrorField::SEVERITY_UNLOCALIZED, severity),
        (ErrorField::CODE, error.sqlstate().to_owned()),
        (ErrorField::MESSAGE, error.to_string()),
    ];
    if let Some(detail) = error.detail() {
        fields.push((ErrorField::DETAIL, detail));
    }
    if let Some(hint) = error.hint() {
        fields.push((ErrorField::HINT, hint.to_owned()));
    }
    if let Some(position) = error.position() {
        fields.push((ErrorField::POSITION, position.to_string()));
    }
    fields
}

/// An error rendered as the message a client receives.
///
/// A [`Severity::Notice`] or [`Severity::Warning`] is a `NoticeResponse`, not an `ErrorResponse` —
/// the difference decides whether the client thinks the statement failed, and PostgreSQL sends
/// some conditions (`COMMIT` outside a transaction, for one) as warnings that do not fail anything.
#[must_use]
pub fn error_message<'a>(error: &SqlError, fields: &'a [(ErrorField, String)]) -> Backend<'a> {
    match error.severity() {
        Severity::Notice | Severity::Warning => Backend::Notice(fields),
        Severity::Error | Severity::Fatal => Backend::Error(fields),
    }
}

/// What to do with the version and options a client asked for in its startup packet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Negotiation {
    /// The client asked for exactly what this server speaks; say nothing and carry on.
    Proceed,
    /// Send a `NegotiateProtocolVersion` first, then carry on with the ordinary sequence.
    Downgrade {
        /// `_pq_.` options the client asked for that this server does not implement.
        unsupported_options: Vec<String>,
    },
    /// The major version is not 3. There is no negotiating this one — the framing itself differs.
    Unsupported {
        /// What the client asked for.
        major: u16,
    },
}

/// Decides how to answer a startup packet's version and options.
///
/// A *newer* minor version is downgraded, and so is any `_pq_.` option we do not know. An *older*
/// minor version needs no message: 3.0 is what we speak, and a client asking for it is already
/// getting what it wants.
#[must_use]
pub fn negotiation(startup: &Startup) -> Negotiation {
    let Startup::Parameters {
        major,
        minor,
        parameters,
    } = startup
    else {
        return Negotiation::Proceed;
    };
    if *major != PROTOCOL_MAJOR {
        return Negotiation::Unsupported { major: *major };
    }
    // Protocol options are startup parameters whose names begin with `_pq_.`; a server reports the
    // ones it does not implement, which for this server is all of them.
    let unsupported_options: Vec<String> = parameters
        .iter()
        .filter(|(name, _)| name.starts_with("_pq_."))
        .map(|(name, _)| name.clone())
        .collect();
    if *minor > PROTOCOL_MINOR || !unsupported_options.is_empty() {
        Negotiation::Downgrade {
            unsupported_options,
        }
    } else {
        Negotiation::Proceed
    }
}

#[cfg(test)]
mod tests {
    use super::{Negotiation, error_fields, error_message, negotiation};
    use crate::error::SqlError;
    use crate::pgwire::message::{Backend, ErrorField, Startup};
    use crate::sqlstate;

    fn startup(minor: u16, parameters: &[(&str, &str)]) -> Startup {
        Startup::Parameters {
            major: 3,
            minor,
            parameters: parameters
                .iter()
                .map(|(n, v)| ((*n).to_owned(), (*v).to_owned()))
                .collect(),
        }
    }

    #[test]
    fn the_version_this_server_speaks_needs_no_negotiation() {
        assert_eq!(
            negotiation(&startup(0, &[("user", "esker")])),
            Negotiation::Proceed
        );
    }

    /// The case that decides whether a current `libpq` can connect at all. It asks for 3.2; a
    /// server that errors instead of downgrading is unreachable.
    #[test]
    fn a_newer_minor_version_is_downgraded_rather_than_refused() {
        assert_eq!(
            negotiation(&startup(2, &[("user", "esker")])),
            Negotiation::Downgrade {
                unsupported_options: Vec::new()
            }
        );
    }

    /// PostgreSQL 19 lists unknown `_pq_.` options by name in the same message, so we do too.
    #[test]
    fn unknown_protocol_options_are_reported_by_name() {
        let asked = startup(
            0,
            &[
                ("user", "esker"),
                ("_pq_.made_up_thing", "on"),
                ("application_name", "psql"),
            ],
        );
        assert_eq!(
            negotiation(&asked),
            Negotiation::Downgrade {
                unsupported_options: vec!["_pq_.made_up_thing".to_owned()]
            }
        );
    }

    /// A different major version is a different protocol; its framing does not even match.
    #[test]
    fn another_major_version_is_not_negotiable() {
        let asked = Startup::Parameters {
            major: 2,
            minor: 0,
            parameters: Vec::new(),
        };
        assert_eq!(negotiation(&asked), Negotiation::Unsupported { major: 2 });
    }

    /// Both severity fields, or a driver that reads the unlocalised one sees nothing.
    #[test]
    fn an_error_carries_severity_twice_and_the_code_once() {
        let error = SqlError::UndefinedTable("nope".into());
        let fields = error_fields(&error);
        assert_eq!(fields[0], (ErrorField::SEVERITY, "ERROR".to_owned()));
        assert_eq!(
            fields[1],
            (ErrorField::SEVERITY_UNLOCALIZED, "ERROR".to_owned())
        );
        assert_eq!(
            fields[2],
            (ErrorField::CODE, sqlstate::UNDEFINED_TABLE.to_owned())
        );
        assert_eq!(
            fields[3],
            (
                ErrorField::MESSAGE,
                "relation \"nope\" does not exist".to_owned()
            )
        );
    }

    /// A warning must not arrive as an `ErrorResponse`: the client would think the statement
    /// failed, and PostgreSQL's own answer to `COMMIT` outside a transaction is a warning.
    #[test]
    fn a_warning_is_a_notice_and_not_an_error() {
        let error = SqlError::NoActiveTransaction;
        let fields = error_fields(&error);
        assert!(matches!(error_message(&error, &fields), Backend::Notice(_)));
        let failure = SqlError::UndefinedTable("t".into());
        let failure_fields = error_fields(&failure);
        assert!(matches!(
            error_message(&failure, &failure_fields),
            Backend::Error(_)
        ));
    }

    /// A syntax error carries the caret position `psql` draws under the offending token.
    #[test]
    fn a_syntax_error_carries_its_position() {
        let error = SqlError::Syntax {
            message: "bad".into(),
            position: Some(15),
            hint: None,
        };
        let fields = error_fields(&error);
        assert_eq!(
            fields.last(),
            Some(&(ErrorField::POSITION, "15".to_owned()))
        );
    }
}
