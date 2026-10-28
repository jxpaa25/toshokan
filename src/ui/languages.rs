//! The languages MangaDex translates into, by code.

/// MangaDex language codes and their English names, most common first.
pub const LANGUAGES: [(&str, &str); 59] = [
    ("en", "English"),
    ("ja", "Japanese"),
    ("ja-ro", "Japanese (romaji)"),
    ("ko", "Korean"),
    ("ko-ro", "Korean (romanized)"),
    ("zh", "Chinese (simplified)"),
    ("zh-hk", "Chinese (traditional)"),
    ("zh-ro", "Chinese (romanized)"),
    ("es", "Spanish"),
    ("es-la", "Spanish (LATAM)"),
    ("pt", "Portuguese"),
    ("pt-br", "Portuguese (Brazil)"),
    ("fr", "French"),
    ("de", "German"),
    ("it", "Italian"),
    ("nl", "Dutch"),
    ("ru", "Russian"),
    ("uk", "Ukrainian"),
    ("be", "Belarusian"),
    ("pl", "Polish"),
    ("cs", "Czech"),
    ("sk", "Slovak"),
    ("hu", "Hungarian"),
    ("ro", "Romanian"),
    ("bg", "Bulgarian"),
    ("sr", "Serbian"),
    ("hr", "Croatian"),
    ("el", "Greek"),
    ("tr", "Turkish"),
    ("ar", "Arabic"),
    ("fa", "Persian"),
    ("he", "Hebrew"),
    ("hi", "Hindi"),
    ("bn", "Bengali"),
    ("ne", "Nepali"),
    ("ta", "Tamil"),
    ("th", "Thai"),
    ("vi", "Vietnamese"),
    ("id", "Indonesian"),
    ("ms", "Malay"),
    ("tl", "Filipino"),
    ("my", "Burmese"),
    ("mn", "Mongolian"),
    ("kk", "Kazakh"),
    ("uz", "Uzbek"),
    ("az", "Azerbaijani"),
    ("ka", "Georgian"),
    ("lt", "Lithuanian"),
    ("lv", "Latvian"),
    ("et", "Estonian"),
    ("fi", "Finnish"),
    ("sv", "Swedish"),
    ("no", "Norwegian"),
    ("da", "Danish"),
    ("ca", "Catalan"),
    ("eu", "Basque"),
    ("gl", "Galician"),
    ("la", "Latin"),
    ("eo", "Esperanto"),
];

/// English name for a MangaDex language code, or the code itself when unknown.
pub fn language_name(code: &str) -> &str {
    LANGUAGES
        .iter()
        .find(|(known, _)| *known == code)
        .map_or(code, |(_, name)| name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_known_codes_and_keeps_unknown_ones() {
        assert_eq!(language_name("es-la"), "Spanish (LATAM)");
        assert_eq!(language_name("xx"), "xx");
    }
}
