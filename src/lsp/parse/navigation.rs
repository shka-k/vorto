//! Definition / declaration / references navigation: `Location`,
//! `LocationLink`, and the flat-list normaliser used by every
//! `textDocument/{definition,declaration,typeDefinition,references}`
//! response shape.

use serde_json::Value;

use super::parse_range;
use crate::lsp::types::Location;

fn parse_location(v: &Value) -> Option<Location> {
    let uri = v.get("uri").and_then(|x| x.as_str())?.to_string();
    let range = parse_range(v.get("range")?)?;
    Some(Location { uri, range })
}

/// Parse a `LocationLink` and reduce it to the same shape as `Location`
/// (taking `targetUri` + `targetSelectionRange`).
fn parse_location_link(v: &Value) -> Option<Location> {
    let uri = v.get("targetUri").and_then(|x| x.as_str())?.to_string();
    let range = parse_range(
        v.get("targetSelectionRange")
            .or_else(|| v.get("targetRange"))?,
    )?;
    Some(Location { uri, range })
}

/// `textDocument/definition` may answer with a single `Location`, a
/// single `LocationLink`, an array of either, or `null`. Normalise to a
/// flat `Vec<Location>`.
pub fn parse_locations(v: &Value) -> Vec<Location> {
    if v.is_null() {
        return Vec::new();
    }
    if let Some(arr) = v.as_array() {
        return arr
            .iter()
            .filter_map(|x| parse_location(x).or_else(|| parse_location_link(x)))
            .collect();
    }
    if let Some(loc) = parse_location(v).or_else(|| parse_location_link(v)) {
        return vec![loc];
    }
    Vec::new()
}
