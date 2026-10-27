use std::{collections::HashMap, sync::OnceLock};

/// Bundled language codes. The catalog is selected from the locale's primary subtag.
fn language_code(locale: &str) -> Option<&'static str> {
    match locale.split([':', '_', '-', '.']).next()? {
        "el" => Some("el"),
        "es" => Some("es"),
        "it" => Some("it"),
        "pt" => Some("pt"),
        "fr" => Some("fr"),
        "de" => Some("de"),
        _ => None,
    }
}

fn catalog(language: &str) -> &'static HashMap<String, String> {
    static CATALOGS: OnceLock<HashMap<&'static str, HashMap<String, String>>> = OnceLock::new();
    let catalogs = CATALOGS.get_or_init(|| {
        [
            ("el", include_str!("../locales/el.yaml")),
            ("es", include_str!("../locales/es.yaml")),
            ("it", include_str!("../locales/it.yaml")),
            ("pt", include_str!("../locales/pt.yaml")),
            ("fr", include_str!("../locales/fr.yaml")),
            ("de", include_str!("../locales/de.yaml")),
        ]
        .into_iter()
        .map(|(code, source)| {
            let translations = serde_yaml::from_str(source).expect("valid translation catalog");
            (code, translations)
        })
        .collect()
    });
    catalogs.get(language).expect("bundled translation catalog")
}

pub fn tr(text: &str) -> String {
    static LANGUAGE: OnceLock<Option<&'static str>> = OnceLock::new();
    let language = *LANGUAGE.get_or_init(|| {
        // GLib follows LANGUAGE, LC_ALL, LC_MESSAGES and LANG, in that order.
        let names = glib::language_names();
        preferred_language(
            crate::preferences::Preferences::load().language,
            names.first().map(|name| name.as_str()).unwrap_or("C"),
        )
    });
    let Some(language) = language else {
        return text.to_owned();
    };
    let catalog = catalog(language);
    catalog
        .get(text)
        .cloned()
        .or_else(|| {
            catalog
                .iter()
                .find(|(key, _)| key.to_uppercase() == text)
                .map(|(_, value)| value.to_uppercase())
        })
        .unwrap_or_else(|| text.to_owned())
}

fn preferred_language(
    preference: crate::preferences::Language,
    system: &str,
) -> Option<&'static str> {
    language_code(preference.code().unwrap_or(system))
}

pub fn tray_label(text: &str) -> String {
    let mut result = tr(text);
    for part in [
        "Percentage unavailable",
        "Battery unavailable",
        "Very low battery",
        "Low battery",
        "Charging",
        "Fully charged",
    ] {
        result = result.replace(part, &tr(part));
    }
    result
}

pub fn translate_widget(widget: &gtk4::Widget) {
    use gtk4::prelude::*;
    if let Some(label) = widget.downcast_ref::<gtk4::Label>() {
        let original = label.text();
        let translated = tr(&original);
        if translated != original {
            label.set_text(&translated);
        }
    }
    if let Some(tooltip) = widget.tooltip_text() {
        let translated = tr(&tooltip);
        if translated != tooltip {
            widget.set_tooltip_text(Some(&translated));
        }
    }
    let mut child = widget.first_child();
    while let Some(current) = child {
        translate_widget(&current);
        child = current.next_sibling();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explicit_language_overrides_the_system_without_changing_it() {
        use crate::preferences::Language;
        assert_eq!(
            preferred_language(Language::Automatic, "el_GR.UTF-8"),
            Some("el")
        );
        assert_eq!(preferred_language(Language::English, "el_GR.UTF-8"), None);
        assert_eq!(
            preferred_language(Language::Greek, "en_US.UTF-8"),
            Some("el")
        );
        assert_eq!(preferred_language(Language::Automatic, "ja_JP"), None);
        for language in Language::ALL {
            if let Some(code) = language.code() {
                assert_eq!(preferred_language(language, "C"), language_code(code));
            }
        }
    }

    #[test]
    fn locale_detection_and_catalogs() {
        for locale in ["el", "el_GR.UTF-8", "el-GR"] {
            assert_eq!(language_code(locale), Some("el"));
        }
        for locale in ["en_US", "C", "en-GB"] {
            assert_eq!(language_code(locale), None);
        }
        assert_eq!(language_code("de_DE.UTF-8"), Some("de"));
        assert_eq!(language_code("es_MX"), Some("es"));
        assert_eq!(language_code("it_IT"), Some("it"));
        assert_eq!(language_code("pt_BR.UTF-8"), Some("pt"));
        assert_eq!(language_code("fr_CA"), Some("fr"));

        let reference = catalog("el");
        assert_eq!(reference.get("General").unwrap(), "Γενικά");
        for language in ["el", "es", "it", "pt", "fr", "de"] {
            let translations = catalog(language);
            assert_eq!(
                translations.len(),
                reference.len(),
                "{language}: unexpected key count"
            );
            for (key, _) in reference {
                assert!(
                    translations.contains_key(key),
                    "{language}: missing key {key}"
                );
                assert!(
                    !translations[key].is_empty(),
                    "{language}: empty value for {key}"
                );
            }
        }
    }
}
