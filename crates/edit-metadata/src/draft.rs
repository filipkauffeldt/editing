//! Draft tracking for the metadata form: which fields did the user change
//! compared with what was loaded.

use std::collections::{BTreeMap, BTreeSet};

use crate::fields::FIELDS;
use crate::iptc_read::Merged;
use crate::template::TemplateFields;

/// The fields whose current text differs from the loaded values, as the
/// write set. A blank value means the user cleared the field.
///
/// - `loaded`: per field, the value all photos share or `Mixed`.
/// - `current`: the text now in each widget.
/// - `touched`: fields the user edited; only these count for `Mixed` ones,
///   whose widget starts blank.
///
/// Nothing counts as changed before values are loaded.
pub fn changed_fields(
    loaded: &BTreeMap<&'static str, Merged>,
    current: &BTreeMap<&'static str, String>,
    touched: &BTreeSet<&'static str>,
) -> TemplateFields {
    let mut out = TemplateFields::default();
    for spec in FIELDS {
        let (Some(base), Some(text)) = (loaded.get(spec.id), current.get(spec.id)) else {
            continue;
        };
        let changed = match base {
            Merged::Same(value) => value.trim() != text.trim(),
            Merged::Mixed => touched.contains(spec.id),
        };
        if changed {
            out.set(spec.id, Some(text.trim().to_owned()));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn loaded() -> BTreeMap<&'static str, Merged> {
        BTreeMap::from([
            ("headline", Merged::Same("Old".into())),
            ("credit", Merged::Mixed),
            ("source", Merged::Same(String::new())),
        ])
    }

    fn text(pairs: &[(&'static str, &str)]) -> BTreeMap<&'static str, String> {
        pairs.iter().map(|(k, v)| (*k, (*v).to_owned())).collect()
    }

    #[test]
    fn untouched_form_has_no_changes() {
        let cur = text(&[("headline", "Old"), ("credit", ""), ("source", "")]);
        assert!(changed_fields(&loaded(), &cur, &BTreeSet::new()).is_empty());
    }

    #[test]
    fn only_edited_fields_are_in_the_payload() {
        let cur = text(&[("headline", "New "), ("credit", ""), ("source", "")]);
        let out = changed_fields(&loaded(), &cur, &BTreeSet::new());
        assert_eq!(out, TemplateFields::default().with("headline", "New"));
    }

    #[test]
    fn mixed_field_counts_only_when_touched() {
        let cur = text(&[("headline", "Old"), ("credit", "Anna")]);
        assert!(changed_fields(&loaded(), &cur, &BTreeSet::new()).is_empty());
        let out = changed_fields(&loaded(), &cur, &BTreeSet::from(["credit"]));
        assert_eq!(out, TemplateFields::default().with("credit", "Anna"));
    }

    #[test]
    fn clearing_a_field_is_a_change() {
        let cur = text(&[("headline", "  ")]);
        let out = changed_fields(&loaded(), &cur, &BTreeSet::new());
        assert_eq!(out.get("headline"), Some(""));
        let cleared_mixed = changed_fields(
            &loaded(),
            &text(&[("credit", "")]),
            &BTreeSet::from(["credit"]),
        );
        assert_eq!(cleared_mixed.get("credit"), Some(""));
    }

    #[test]
    fn nothing_changes_before_the_load_finishes() {
        let cur = text(&[("headline", "typed")]);
        assert!(changed_fields(&BTreeMap::new(), &cur, &BTreeSet::from(["headline"])).is_empty());
    }
}
