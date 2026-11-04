//! TSV parsing shared by template generation and runtime translation lookup.
pub(crate) fn entries(catalog: &str) -> impl Iterator<Item = (&str, &str)> {
    catalog.lines().enumerate().filter_map(|(index, line)| {
        if line.trim().is_empty() {
            return None;
        }
        let (key, value) = line.split_once('\t').unwrap_or_else(|| {
            panic!(
                "translation catalog line {}: expected key<TAB>value",
                index + 1
            )
        });
        assert!(
            !key.is_empty() && !value.is_empty(),
            "translation catalog line {}: key and value must be nonempty",
            index + 1,
        );
        Some((key, value))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn blank_separators_and_crlf_preserve_translation_text() {
        let catalog = "Language\t語言\r\n\r\n  \r\nRename\t改名\r\n";
        assert_eq!(
            entries(catalog).collect::<Vec<_>>(),
            vec![("Language", "語言"), ("Rename", "改名")]
        );
    }
    #[test]
    #[should_panic(expected = "translation catalog line 3: expected key<TAB>value")]
    fn malformed_entries_report_the_original_line_number() {
        entries("Language\t語言\n\nbroken entry").for_each(drop);
    }
    #[test]
    #[should_panic(expected = "translation catalog line 2: key and value must be nonempty")]
    fn empty_values_are_not_silently_accepted() {
        entries("\nRename\t").for_each(drop);
    }
}
