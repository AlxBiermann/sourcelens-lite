//! Closed value sets of the schema (`CHECK` constraints and fixed codes) as Rust types.
//! Database and wire values are the `as_str` strings.

use std::fmt;

use rusqlite::types::{FromSql, FromSqlError, FromSqlResult, ToSql, ToSqlOutput, ValueRef};
use serde::{Serialize, Serializer};

/// `chunks.embed_state`: waiting for an embedding.
pub const EMBED_PENDING: i64 = 0;
/// `chunks.embed_state`: has a row in `vec_chunks`.
pub const EMBED_DONE: i64 = 1;
/// `chunks.embed_state`: the provider refused the input.
pub const EMBED_REJECTED: i64 = 2;

macro_rules! text_enum {
    ($(#[$meta:meta])* $name:ident { $($variant:ident => $value:literal),+ $(,)? }) => {
        $(#[$meta])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
        pub enum $name {
            $($variant),+
        }

        impl $name {
            pub fn as_str(self) -> &'static str {
                match self {
                    $(Self::$variant => $value),+
                }
            }

            pub fn parse(value: &str) -> Option<Self> {
                match value {
                    $($value => Some(Self::$variant),)+
                    _ => None,
                }
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(self.as_str())
            }
        }

        impl Serialize for $name {
            fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
                serializer.serialize_str(self.as_str())
            }
        }

        impl ToSql for $name {
            fn to_sql(&self) -> rusqlite::Result<ToSqlOutput<'_>> {
                Ok(ToSqlOutput::from(self.as_str()))
            }
        }

        impl FromSql for $name {
            fn column_result(value: ValueRef<'_>) -> FromSqlResult<Self> {
                let text = value.as_str()?;
                Self::parse(text).ok_or_else(|| {
                    FromSqlError::Other(
                        format!("unknown {} value {text:?}", stringify!($name)).into(),
                    )
                })
            }
        }
    };
}

text_enum! {
    /// `jobs.trigger`: why a job was queued.
    Trigger {
        Connect => "connect",
        Webhook => "webhook",
        Manual => "manual",
        Startup => "startup",
        Settings => "settings",
    }
}

text_enum! {
    /// `jobs.status`.
    JobStatus {
        Queued => "queued",
        Running => "running",
        Succeeded => "succeeded",
        Failed => "failed",
    }
}

text_enum! {
    /// `jobs.phase`.
    JobPhase {
        Resolve => "resolve",
        Index => "index",
        Embed => "embed",
    }
}

text_enum! {
    /// `repositories.state`.
    RepoState {
        Connected => "connected",
        Disconnected => "disconnected",
        Deleting => "deleting",
    }
}

text_enum! {
    /// `users.role`.
    Role {
        Owner => "owner",
        Member => "member",
    }
}

text_enum! {
    /// `files.skip_reason`: why a file has no content in the index.
    SkipReason {
        Binary => "binary",
        Minified => "minified",
        Generated => "generated",
        NotInArchive => "not_in_archive",
    }
}
