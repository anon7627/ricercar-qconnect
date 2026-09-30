//! Settings the plugin declares to the host (`initialize` → `settings`),
//! and the values the host sends back (`initialize` params, then
//! `settings.changed`).

use serde_json::{json, Map, Value};

/// Values in effect.
#[derive(Debug, Clone, PartialEq)]
pub struct Settings {
    /// Tell Qobuz which tracks are played (`track/reportStreaming*`), as its
    /// apps do: plays count for the artists and the account's history.
    pub report_playback: bool,
    /// Fetch streams as encrypted CMAF segments (`file/url`), decrypted by a
    /// local relay, instead of plain `track/getFileUrl` URLs.
    pub cmaf: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Settings { report_playback: true, cmaf: false }
    }
}

impl Settings {
    /// Values from the host's `{key: value}` map. Unknown keys and values of
    /// the wrong type are ignored; missing keys keep their default.
    pub fn from_values(values: Option<&Value>) -> Self {
        let mut s = Settings::default();
        let Some(values) = values.and_then(Value::as_object) else { return s };
        let flag = |values: &Map<String, Value>, key: &str| values.get(key).and_then(Value::as_bool);
        if let Some(on) = flag(values, "report_playback") {
            s.report_playback = on;
        }
        if let Some(on) = flag(values, "cmaf") {
            s.cmaf = on;
        }
        s
    }
}

/// The declaration, labelled in `lang` (French, else English).
pub fn declaration(lang: &str) -> Value {
    let fr = lang == "fr";
    let t = |en: &'static str, fr_text: &'static str| if fr { fr_text } else { en };
    let d = Settings::default();
    json!([
        {
            "key": "report_playback", "type": "bool", "default": d.report_playback,
            "section": t("Playback", "Lecture"),
            "label": t("Report plays to Qobuz", "Signaler les écoutes à Qobuz"),
            "description": t(
                "Tell Qobuz which tracks you play and for how long, as its own apps do. \
                 Plays then count for the artists and appear in your Qobuz history.",
                "Indique à Qobuz les titres écoutés et leur durée d'écoute, comme le font ses \
                 applications. Les écoutes comptent alors pour les artistes et apparaissent dans \
                 votre historique Qobuz."
            ),
        },
        {
            "key": "cmaf", "type": "bool", "default": d.cmaf,
            "section": t("Playback", "Lecture"),
            "label": t("Encrypted streaming (CMAF)", "Lecture chiffrée (CMAF)"),
            "description": t(
                "Fetch tracks the way the current Qobuz web player does: encrypted segments, \
                 decrypted on this computer and handed to the player as the original FLAC, \
                 unchanged. Use it if tracks stop playing with the default method. Applies from \
                 the next track.",
                "Récupère les titres comme le lecteur web Qobuz actuel : des segments chiffrés, \
                 déchiffrés sur cet ordinateur et transmis au lecteur dans leur FLAC d'origine, \
                 sans modification. À activer si les titres ne se lancent plus avec la méthode \
                 par défaut. S'applique à partir du titre suivant."
            ),
        },
    ])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn values_fall_back_to_defaults() {
        assert_eq!(Settings::from_values(None), Settings::default());
        assert!(Settings::default().report_playback && !Settings::default().cmaf);
        let s = Settings::from_values(Some(&json!({"report_playback": false, "cmaf": true, "gone": 3})));
        assert_eq!(s, Settings { report_playback: false, cmaf: true });
        let s = Settings::from_values(Some(&json!({"report_playback": "no"})));
        assert!(s.report_playback, "a value of the wrong type is ignored");
    }

    #[test]
    fn declaration_is_translated_and_complete() {
        let fr = declaration("fr");
        let en = declaration("de");
        assert_eq!(fr[0]["label"], "Signaler les écoutes à Qobuz");
        assert_eq!(en[0]["label"], "Report plays to Qobuz");
        for entry in fr.as_array().unwrap() {
            assert_eq!(entry["type"], "bool");
            assert!(entry["default"].is_boolean() && entry["description"].is_string());
            assert!(entry.get("restart").is_none(), "applied without restarting the plugin");
        }
        assert_eq!(fr[0]["default"], true);
        assert_eq!(fr[1]["default"], false);
    }
}
