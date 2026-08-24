use std::fmt::Write;

pub fn escape_text(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    for character in value.chars() {
        match character {
            '&' => escaped.push_str("&amp;"),
            '<' => escaped.push_str("&lt;"),
            '>' => escaped.push_str("&gt;"),
            '\'' => escaped.push_str("&apos;"),
            '"' => escaped.push_str("&quot;"),
            '\u{9}'
            | '\u{a}'
            | '\u{d}'
            | '\u{20}'..='\u{d7ff}'
            | '\u{e000}'..='\u{fffd}'
            | '\u{10000}'..='\u{10ffff}' => escaped.push(character),
            _ => escaped.push('\u{fffd}'),
        }
    }
    escaped
}

pub fn element(output: &mut String, name: &str, value: &str) {
    let _ = write!(output, "<{name}>{}</{name}>", escape_text(value));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escapes_markup_and_replaces_invalid_xml_characters() {
        assert_eq!(escape_text("a&<\u{1}b"), "a&amp;&lt;�b");
    }
}
