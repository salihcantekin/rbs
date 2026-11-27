//! Where a foldable section is stored in the workspace (ADR-0078), and the match the
//! Properties search field asks of a section's title.

/// Sections that start closed (ADR-0078): the workspace lists them when the user
/// opens them, under the id with this suffix, so an old file keeps them closed.
pub(crate) const CLOSED_BY_DEFAULT: [&str; 8] = [
    "properties.options",
    "sources.global",
    "mixer.hidden",
    "mixer.loudness",
    "properties.text-outline",
    "properties.text-shadow",
    "properties.text-background",
    "properties.scroll",
];
pub(crate) const OPENED_SUFFIX: &str = "-open";

/// The key a section is stored under, and whether listing it means "closed".
pub(crate) fn section_key(id: &str) -> (String, bool) {
    if CLOSED_BY_DEFAULT.contains(&id) {
        (format!("{id}{OPENED_SUFFIX}"), false)
    } else {
        (id.to_owned(), true)
    }
}

/// Does a section called `title` answer the Properties search `query`? Case does not
/// matter and every word of the query must be in the title; an empty query matches
/// nothing (it is no search).
pub(crate) fn matches(title: &str, query: &str) -> bool {
    let title = title.to_lowercase();
    let mut words = query.split_whitespace().peekable();
    words.peek().is_some() && words.all(|w| title.contains(&w.to_lowercase()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_title_answers_words_in_any_case_and_order() {
        assert!(matches("Position and size", "SIZE pos"));
        assert!(matches("Crop", "cr"));
        assert!(!matches("Crop", "shadow"));
    }

    #[test]
    fn an_empty_query_matches_nothing() {
        assert!(!matches("Crop", ""));
        assert!(!matches("Crop", "   "));
    }

    #[test]
    fn a_section_that_starts_closed_is_stored_when_opened() {
        assert_eq!(
            section_key("properties.options"),
            ("properties.options-open".to_owned(), false)
        );
        assert_eq!(
            section_key("properties.crop"),
            ("properties.crop".to_owned(), true)
        );
    }
}
