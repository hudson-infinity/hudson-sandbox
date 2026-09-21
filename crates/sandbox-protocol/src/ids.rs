//! Public resource identifiers.
//!
//! Every public identity is a short resource prefix followed by a canonical
//! lowercase UUIDv7, as specified in `docs/data-models.md`. The prefix exists
//! so a reader can tell resources apart in logs and responses; the UUID is
//! what PostgreSQL stores. The prefix is attached and stripped at the API
//! boundary, and there is no public-to-internal mapping table.
//!
//! These are opaque identifiers, never bearer credentials. A caller knowing an
//! identifier grants nothing: authorization applies to every lookup.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Why a string could not be read as a typed identifier.
///
/// The variants distinguish a wrong resource type from a malformed value so a
/// caller receives an accurate `400`, and so a log line says which it was.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum IdParseError {
    /// The value carried a different resource prefix than the one expected.
    #[error("expected prefix `{expected}_`, found `{found}`")]
    WrongPrefix {
        /// The prefix the caller's target type requires.
        expected: &'static str,
        /// What the value actually started with, truncated at the separator.
        found: String,
    },
    /// No `_` separator, so there is no prefix to check.
    #[error("missing `_` separator between prefix and UUID")]
    MissingSeparator,
    /// The portion after the separator is not a canonical UUID.
    #[error("`{0}` is not a canonical lowercase UUID")]
    MalformedUuid(String),
    /// The UUID parsed but is not version 7.
    #[error("expected a UUIDv7, found version {0}")]
    WrongUuidVersion(usize),
}

/// A prefixed, typed resource identifier.
///
/// Implemented by each identity type through [`define_id`] so that a
/// `SandboxId` cannot be passed where an `OperationId` belongs.
pub trait Id: Sized {
    /// The resource prefix, without the trailing underscore.
    const PREFIX: &'static str;

    /// The underlying UUID, as stored in PostgreSQL.
    fn uuid(&self) -> Uuid;

    /// Wrap an existing UUID without validating its version.
    ///
    /// Use this for values read back from the database, which were validated
    /// when they were generated. Use [`Id::generate`] for new identities and
    /// `parse` for anything arriving from a caller.
    fn from_uuid(uuid: Uuid) -> Self;

    /// Mint a new identity.
    fn generate() -> Self {
        Self::from_uuid(Uuid::now_v7())
    }
}

/// Parse the shared `prefix_uuid` shape, checking the prefix and UUID version.
fn parse_prefixed(value: &str, expected: &'static str) -> Result<Uuid, IdParseError> {
    let (prefix, rest) = value
        .split_once('_')
        .ok_or(IdParseError::MissingSeparator)?;

    if prefix != expected {
        return Err(IdParseError::WrongPrefix {
            expected,
            found: prefix.to_owned(),
        });
    }

    // Reject alternate spellings — uppercase, braces, urn: — by requiring the
    // input to match the canonical rendering of what it parsed to.
    let uuid = Uuid::try_parse(rest).map_err(|_| IdParseError::MalformedUuid(rest.to_owned()))?;
    if uuid.hyphenated().to_string() != rest {
        return Err(IdParseError::MalformedUuid(rest.to_owned()));
    }

    let version = uuid.get_version_num();
    if version != 7 {
        return Err(IdParseError::WrongUuidVersion(version));
    }

    Ok(uuid)
}

macro_rules! define_id {
    ($(#[$meta:meta])* $name:ident, $prefix:literal) => {
        $(#[$meta])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
        #[serde(try_from = "String", into = "String")]
        pub struct $name(Uuid);

        impl Id for $name {
            const PREFIX: &'static str = $prefix;

            fn uuid(&self) -> Uuid {
                self.0
            }

            fn from_uuid(uuid: Uuid) -> Self {
                Self(uuid)
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "{}_{}", $prefix, self.0.hyphenated())
            }
        }

        impl FromStr for $name {
            type Err = IdParseError;

            fn from_str(value: &str) -> Result<Self, Self::Err> {
                parse_prefixed(value, $prefix).map(Self)
            }
        }

        impl TryFrom<String> for $name {
            type Error = IdParseError;

            fn try_from(value: String) -> Result<Self, Self::Error> {
                value.parse()
            }
        }

        impl From<$name> for String {
            fn from(id: $name) -> Self {
                id.to_string()
            }
        }
    };
}

define_id!(
    /// A tenant. The ownership boundary for every other resource.
    ProjectId,
    "prj"
);
define_id!(
    /// A sandbox. Keeps one identity across pause and resume, and is never
    /// reused after destroy.
    SandboxId,
    "sbx"
);
define_id!(
    /// One admitted mutating request.
    OperationId,
    "op"
);
define_id!(
    /// One immutable, completed save of memory, disk, and VM state.
    SnapshotId,
    "snp"
);
define_id!(
    /// One sandbox incarnation's resource reservation on a compute host.
    AllocationId,
    "alc"
);
define_id!(
    /// A registered compute machine. Internal; not exposed in ordinary
    /// sandbox responses.
    HostId,
    "hst"
);

#[cfg(test)]
mod tests {
    // Tests assert on known-good values, so a panic on the unexpected case is
    // the assertion. The workspace denies these lints in shipping code.
    #![allow(clippy::expect_used, clippy::unwrap_used)]

    use super::*;

    #[test]
    fn renders_with_its_prefix() {
        let uuid = Uuid::try_parse("01996110-7c00-7000-8000-000000000001").expect("valid uuid");
        let id = SandboxId::from_uuid(uuid);
        assert_eq!(id.to_string(), "sbx_01996110-7c00-7000-8000-000000000001");
    }

    #[test]
    fn round_trips_through_a_string() {
        let id = SandboxId::generate();
        let parsed: SandboxId = id.to_string().parse().expect("round trip");
        assert_eq!(id, parsed);
    }

    #[test]
    fn generates_version_seven() {
        assert_eq!(OperationId::generate().uuid().get_version_num(), 7);
    }

    #[test]
    fn rejects_another_resources_prefix() {
        let sandbox = SandboxId::generate().to_string();
        let err = sandbox.parse::<OperationId>().expect_err("wrong type");
        assert_eq!(
            err,
            IdParseError::WrongPrefix {
                expected: "op",
                found: "sbx".to_owned(),
            }
        );
    }

    #[test]
    fn rejects_a_bare_uuid() {
        let bare = Uuid::now_v7().hyphenated().to_string();
        assert_eq!(
            bare.parse::<SandboxId>().expect_err("no prefix"),
            IdParseError::MissingSeparator
        );
    }

    #[test]
    fn rejects_alternate_spellings() {
        let uuid = Uuid::now_v7();
        for spelling in [
            uuid.hyphenated().to_string().to_uppercase(),
            uuid.simple().to_string(),
            format!("{{{}}}", uuid.hyphenated()),
            format!("urn:uuid:{}", uuid.hyphenated()),
        ] {
            let value = format!("sbx_{spelling}");
            assert!(
                matches!(
                    value.parse::<SandboxId>(),
                    Err(IdParseError::MalformedUuid(_))
                ),
                "accepted non-canonical spelling: {value}"
            );
        }
    }

    #[test]
    fn rejects_a_truncated_value() {
        assert!(matches!(
            "sbx_01996110-7c00-7000-8000".parse::<SandboxId>(),
            Err(IdParseError::MalformedUuid(_))
        ));
    }

    #[test]
    fn rejects_a_uuid_of_the_wrong_version() {
        // A v4 UUID is well formed but carries no timestamp, so it is not one
        // of ours and must not be accepted from a caller.
        let v4 = Uuid::try_parse("f47ac10b-58cc-4372-a567-0e02b2c3d479").expect("valid uuid");
        let value = format!("sbx_{}", v4.hyphenated());
        assert_eq!(
            value.parse::<SandboxId>().expect_err("wrong version"),
            IdParseError::WrongUuidVersion(4)
        );
    }

    #[test]
    fn serializes_as_its_prefixed_string() {
        let id = ProjectId::generate();
        let json = serde_json::to_string(&id).expect("serialize");
        assert_eq!(json, format!("\"{id}\""));
        let back: ProjectId = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(id, back);
    }
}
