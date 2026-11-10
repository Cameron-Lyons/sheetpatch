//! Streaming validation and selection of package relationships.
use super::namespaces::PACKAGE_NAMESPACE;
use super::part::{self, Tag};
use crate::{Error, Result};
use std::borrow::Cow;
use std::collections::HashSet;

#[cfg(test)]
#[path = "tests/relationships.rs"]
mod tests;

#[cfg_attr(test, derive(Clone, Debug, PartialEq, Eq))]
pub(super) struct Relationship {
    pub(super) id: String,
    pub(super) kind: String,
    pub(super) target: String,
    pub(super) external: bool,
}

/// Validates a whole relationships part and returns, in document order, the
/// relationships that `keep` selects by their ID and type. The others are
/// not copied: only their IDs are kept while the part is parsed, to find
/// duplicates, borrowed from `bytes` unless XML normalization changed them.
// `keep` is a trait object so that every caller's filter shares one copy of
// the part parser; it runs only once for each relationship.
pub(super) fn parse(bytes: &[u8], keep: &dyn Fn(&str, &str) -> bool) -> Result<Vec<Relationship>> {
    // Structural errors are reported only once the whole part is known to be
    // well-formed: an invalid root, then a relationship outside it, then the
    // first relationship with invalid attributes.
    let mut valid_root = false;
    let mut misplaced = false;
    let mut invalid = None;
    let mut ids: HashSet<Cow<'_, str>> = HashSet::new();
    let mut kept = Vec::new();
    part::walk_part(bytes, |tag, _: Option<&()>| {
        if tag.depth == 0 {
            valid_root = tag.local == "Relationships" && tag.namespace() == PACKAGE_NAMESPACE;
        } else if valid_root && tag.local == "Relationship" && tag.namespace() == PACKAGE_NAMESPACE
        {
            if tag.depth > 1 {
                misplaced = true;
            } else if !misplaced && invalid.is_none() {
                match relationship(tag, &ids, keep) {
                    Ok((id, relationship)) => {
                        kept.extend(relationship);
                        ids.insert(id);
                    }
                    Err(error) => invalid = Some(error),
                }
            }
        }
        Ok(())
    })?;
    if !valid_root {
        return Err(Error::InvalidWorkbook("invalid relationships root".into()));
    }
    if misplaced {
        return Err(Error::InvalidWorkbook(
            "relationship outside relationships root".into(),
        ));
    }
    match invalid {
        Some(error) => Err(error),
        None => Ok(kept),
    }
}

// Checks a relationship element's attributes against the IDs before it.
// Returns its ID, and the relationship if `keep` selects it.
fn relationship<'xml>(
    tag: &Tag<'_, 'xml>,
    ids: &HashSet<Cow<'xml, str>>,
    keep: &dyn Fn(&str, &str) -> bool,
) -> Result<(Cow<'xml, str>, Option<Relationship>)> {
    let required = |key| {
        tag.attribute(key)
            .ok_or_else(|| Error::InvalidWorkbook(format!("relationship lacks {key}")))
    };
    let id = required("Id")?;
    if ids.contains(&*id) {
        return Err(Error::InvalidWorkbook("duplicate relationship id".into()));
    }
    let external = match tag.attribute("TargetMode").as_deref() {
        None | Some("Internal") => false,
        Some("External") => true,
        Some(_) => {
            return Err(Error::InvalidWorkbook(
                "invalid relationship TargetMode".into(),
            ));
        }
    };
    let kind = required("Type")?;
    let target = required("Target")?;
    let relationship = keep(&id, &kind).then(|| Relationship {
        id: id.to_string(),
        kind: kind.into_owned(),
        target: target.into_owned(),
        external,
    });
    Ok((id, relationship))
}
