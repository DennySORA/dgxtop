//! Immutable text for high-volume record fields (provenance driver, device
//! path and channel). Thousands of observations per second carry and clone
//! these values: clones share one allocation, and decoding shares repeated
//! values through a bounded per-thread cache. The JSON form is a plain
//! string; length rules stay with the owning type's `validate`.

use std::borrow::Borrow;
use std::cell::RefCell;
use std::collections::HashSet;
use std::fmt;
use std::marker::PhantomData;
use std::ops::Deref;
use std::sync::Arc;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::identity::{IdentityVisitor, InternCache, intern};

/// Shared immutable text; see the module documentation.
#[derive(Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct SharedText(Arc<str>);

impl SharedText {
    pub fn new(value: impl AsRef<str>) -> Self {
        Self(Arc::from(value.as_ref()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Decoding path: one allocation per distinct value.
    fn decoded(value: &str) -> Result<Self, std::convert::Infallible> {
        thread_local! {
            static CACHE: InternCache = RefCell::new(HashSet::new());
        }
        intern(&CACHE, value, |_| Ok(())).map(Self)
    }
}

impl Default for SharedText {
    fn default() -> Self {
        Self::new("")
    }
}

impl Deref for SharedText {
    type Target = str;

    fn deref(&self) -> &str {
        &self.0
    }
}

impl AsRef<str> for SharedText {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl Borrow<str> for SharedText {
    fn borrow(&self) -> &str {
        &self.0
    }
}

impl From<&str> for SharedText {
    fn from(value: &str) -> Self {
        Self::new(value)
    }
}

impl From<String> for SharedText {
    fn from(value: String) -> Self {
        Self(Arc::from(value))
    }
}

impl From<Arc<str>> for SharedText {
    /// Wraps an already shared string without copying it.
    fn from(value: Arc<str>) -> Self {
        Self(value)
    }
}

impl From<&String> for SharedText {
    fn from(value: &String) -> Self {
        Self::new(value)
    }
}

impl From<SharedText> for String {
    fn from(value: SharedText) -> Self {
        value.0.to_string()
    }
}

impl PartialEq<str> for SharedText {
    fn eq(&self, other: &str) -> bool {
        &*self.0 == other
    }
}

impl PartialEq<&str> for SharedText {
    fn eq(&self, other: &&str) -> bool {
        &*self.0 == *other
    }
}

impl PartialEq<String> for SharedText {
    fn eq(&self, other: &String) -> bool {
        &*self.0 == other.as_str()
    }
}

impl fmt::Debug for SharedText {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(&*self.0, formatter)
    }
}

impl fmt::Display for SharedText {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl Serialize for SharedText {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for SharedText {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_str(IdentityVisitor(Self::decoded, PhantomData))
    }
}
