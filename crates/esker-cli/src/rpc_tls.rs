//! The three RPC TLS flags, shared by `esker server` and `esker pd serve`.
//!
//! One type rather than two copies, because the two commands must describe the same thing the same
//! way: an operator who learned `--rpc-tls-cert` on a store should not find it spelled differently
//! on a placement driver, and a refusal an operator meets once should read identically the second
//! time.
//!
//! # What these do, and what they deliberately do not
//!
//! They turn on TLS for the **framed RPC** ([ADR 0055](../../../docs/adr/0055-the-tls-options-across-three-surfaces-measured.md)),
//! which is a different surface from the PostgreSQL port's `--tls-cert`/`--tls-key`: a SQL node
//! terminates client TLS, and these encrypt the links between stores, between placement drivers,
//! and from a client to a store. A deployment may want either, both, or neither.
//!
//! `--rpc-tls-mutual` adds client certificates in both directions, which is the setting for a
//! cluster's internal links where both ends are ours. It is **not** authorisation: a verified
//! certificate says the peer holds a key this cluster's CA vouched for, not that it may register
//! as store 7. Nothing here maps identity to a right yet, and ADR 0055 says so in the same words.

use std::path::PathBuf;

use esker_proto::transport::RpcTls;

/// What a node was told on its command line about RPC TLS.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct RpcTlsFlags {
    /// This node's certificate chain, in PEM.
    pub(crate) cert: Option<PathBuf>,
    /// Its private key, in PEM.
    pub(crate) key: Option<PathBuf>,
    /// The roots that verify the peers it talks to.
    pub(crate) ca: Option<PathBuf>,
    /// Require a certificate from peers as well as presenting one.
    pub(crate) mutual: bool,
}

impl RpcTlsFlags {
    /// Turns the flags into a configuration, or says what is wrong with them.
    ///
    /// **All three files or none.** A node given two of them is a typo away from serving in the
    /// clear on a link an operator believes is encrypted, so it refuses to start rather than
    /// guessing which of the three was meant. `--rpc-tls-mutual` on its own is the same mistake
    /// with a friendlier spelling and gets the same answer.
    ///
    /// # Errors
    ///
    /// An incomplete set, or anything [`RpcTls::from_files`] refuses — including a build without
    /// the `tls` feature, which is a startup error naming the feature rather than a node that
    /// comes up unencrypted.
    pub(crate) fn build(&self) -> Result<RpcTls, String> {
        match (&self.cert, &self.key, &self.ca) {
            (Some(cert), Some(key), Some(ca)) => {
                RpcTls::from_files(cert, key, ca, self.mutual).map_err(|error| error.to_string())
            }
            (None, None, None) if !self.mutual => Ok(RpcTls::disabled()),
            (None, None, None) => Err(
                "--rpc-tls-mutual needs --rpc-tls-cert, --rpc-tls-key and --rpc-tls-ca: there is \
                 no certificate to present without them"
                    .to_owned(),
            ),
            (cert, key, ca) => {
                let mut missing = Vec::new();
                if cert.is_none() {
                    missing.push("--rpc-tls-cert");
                }
                if key.is_none() {
                    missing.push("--rpc-tls-key");
                }
                if ca.is_none() {
                    missing.push("--rpc-tls-ca");
                }
                Err(format!(
                    "RPC TLS needs a certificate, a key and a CA: {} {} missing. Give all three, \
                     or none to speak in the clear",
                    missing.join(" and "),
                    if missing.len() == 1 { "is" } else { "are" },
                ))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::RpcTlsFlags;
    use std::path::PathBuf;

    /// Wrapping is the point: every field under test is an `Option<PathBuf>`.
    #[expect(
        clippy::unnecessary_wraps,
        reason = "the fields it builds are optional"
    )]
    fn some(path: &str) -> Option<PathBuf> {
        Some(PathBuf::from(path))
    }

    #[test]
    fn no_flags_is_no_tls() {
        let flags = RpcTlsFlags::default();
        assert!(
            !flags
                .build()
                .expect("no flags is not an error")
                .is_enabled()
        );
    }

    /// Two of the three is the typo that would otherwise serve in the clear on a link an operator
    /// believes is encrypted. It names what is missing rather than guessing.
    #[test]
    fn an_incomplete_set_is_refused_by_name() {
        let flags = RpcTlsFlags {
            cert: some("/c.pem"),
            key: some("/k.pem"),
            ca: None,
            mutual: false,
        };
        let Err(message) = flags.build() else {
            panic!("two of three must be refused");
        };
        assert!(message.contains("--rpc-tls-ca"), "{message}");
        assert!(message.contains("is missing"), "{message}");

        let one = RpcTlsFlags {
            cert: some("/c.pem"),
            ..RpcTlsFlags::default()
        };
        let Err(message) = one.build() else {
            panic!("one of three must be refused");
        };
        assert!(message.contains("--rpc-tls-key"), "{message}");
        assert!(message.contains("are missing"), "{message}");
    }

    /// Asking for mutual auth with nothing to present is the same mistake spelled differently.
    #[test]
    fn mutual_without_certificates_is_refused() {
        let flags = RpcTlsFlags {
            mutual: true,
            ..RpcTlsFlags::default()
        };
        let Err(message) = flags.build() else {
            panic!("mutual auth with no certificate must be refused");
        };
        assert!(message.contains("--rpc-tls-cert"), "{message}");
    }

    /// A complete set reaches the layer that reads the files — which, in a build without the
    /// feature, is where it is refused for naming the feature.
    #[test]
    fn a_complete_set_is_handed_on() {
        let flags = RpcTlsFlags {
            cert: some("/nonexistent-cert.pem"),
            key: some("/nonexistent-key.pem"),
            ca: some("/nonexistent-ca.pem"),
            mutual: true,
        };
        let Err(message) = flags.build() else {
            panic!("nonexistent files must be refused somewhere");
        };
        #[cfg(not(feature = "tls"))]
        assert!(message.contains("--features tls"), "{message}");
        #[cfg(feature = "tls")]
        assert!(message.contains("nonexistent"), "{message}");
    }
}
