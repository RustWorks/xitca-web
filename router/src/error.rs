use core::{fmt, str::Utf8Error};

use super::tree::{denormalize_params, Node};

/// Represents errors that can occur when inserting a new route.
#[non_exhaustive]
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum InsertError {
    /// Attempted to insert a path that conflicts with an existing route.
    Conflict {
        /// The existing route that the insertion is conflicting with.
        with: String,
    },
    /// Route path is not in utf-8 format.
    Parse(Utf8Error),
    /// Only one parameter per route segment is allowed.
    TooManyParams,
    /// Parameters must be registered with a name.
    UnnamedParam,
    /// Catch-all parameters are only allowed at the end of a path.
    InvalidCatchAll,
}

impl fmt::Display for InsertError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Conflict { with } => {
                write!(
                    f,
                    "insertion failed due to conflict with previously registered route: {with}",
                )
            }
            Self::Parse(ref e) => fmt::Display::fmt(e, f),
            Self::TooManyParams => f.write_str("only one parameter is allowed per path segment"),
            Self::UnnamedParam => f.write_str("parameters must be registered with a name"),
            Self::InvalidCatchAll => f.write_str("catch-all parameters are only allowed at the end of a route"),
        }
    }
}

impl std::error::Error for InsertError {}

impl From<Utf8Error> for InsertError {
    fn from(e: Utf8Error) -> Self {
        Self::Parse(e)
    }
}

impl InsertError {
    pub(crate) fn conflict<T>(route: &[u8], prefix: &[u8], current: &Node<T>) -> Self {
        let mut route = route[..route.len() - prefix.len()].to_owned();

        if !route.ends_with(current.prefix.as_bytes()) {
            route.extend_from_slice(current.prefix.as_bytes());
        }

        let mut last = current;
        while let Some(node) = last.children.first() {
            last = node;
        }

        let mut current = current.children.first();
        while let Some(node) = current {
            route.extend_from_slice(node.prefix.as_bytes());
            current = node.children.first();
        }

        denormalize_params(&mut route, &last.param_remapping);

        InsertError::Conflict {
            with: String::from_utf8(route).unwrap(),
        }
    }
}

/// error type indicate Router can not find a matching route.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub struct MatchError;

impl fmt::Display for MatchError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("router error: route not found")
    }
}

impl std::error::Error for MatchError {}
