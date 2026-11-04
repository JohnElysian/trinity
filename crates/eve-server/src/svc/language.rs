//! Language selection metadata. Clients load translations from their own resources.

use eve_marshal::Value;

use crate::services::{Call, Ctx, Outcome};
use crate::svc::rows::rowset;

// Languages shipped with the original 45017 client, plus implicit English.
const LANGUAGES: [(&str, &str, &str); 9] = [
    ("EN", "English", "English"),
    ("DE", "German", "Deutsch"),
    ("ES", "Spanish", "Español"),
    ("FR", "French", "Français"),
    ("IT", "Italian", "Italiano"),
    ("JA", "Japanese", "日本語"),
    ("KO", "Korean", "한국어"),
    ("PT", "Portuguese", "Português"),
    ("RU", "Russian", "Русский"),
];

/// A supported client language ID, normalized to its canonical form.
pub fn canonical_language(language: &str) -> Option<&'static str> {
    LANGUAGES.iter().map(|(id, _, _)| *id).find(|id| language.eq_ignore_ascii_case(id))
}

/// The client falls back to its implicit English text for unsupported languages.
pub fn resolve_language(language: &str) -> &'static str {
    canonical_language(language).unwrap_or("EN")
}

fn languages() -> Value {
    let lines = LANGUAGES.iter().map(|(id, name, translated)| Value::List(vec![
        Value::bytes(*id), Value::text(*name), Value::text(*translated),
    ])).collect();
    // gameui.GetLanguages reads row.languageID (not row[0]).
    rowset(&["languageID", "languageName", "translatedLanguageName"], lines)
}

pub async fn language_svc(call: &Call, _ctx: &Ctx<'_>) -> Option<Outcome> {
    reply(call)
}

fn reply(call: &Call) -> Option<Outcome> {
    Some(match call.method.as_str() {
        "GetLanguages" if call.args.is_empty() && call.kwargs.is_empty() => Outcome::Value(languages()),
        "GetLanguages" => Outcome::custom("This language method takes no arguments."),
        "DbgGetLanguagePack" => Outcome::custom("Debug language downloads are not supported. Translations are loaded from the client's installed resources."),
        // These legacy stubs have no calls in the 45017 source/disassembly.
        "GetMessagesByLanguageID" | "GetTextsForGroup" => Outcome::custom(
            "This language operation is unavailable in this client version."),
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn call(method: &str) -> Call {
        Call { service: "languageSvc".into(), method: method.into(), args: vec![], kwargs: vec![] }
    }

    #[test]
    fn original_language_rpc_shape() {
        let Some(Outcome::Value(Value::Instance { class, state })) = reply(&call("GetLanguages")) else {
            panic!("expected rowset")
        };
        assert_eq!(*class, Value::bytes("util.Rowset"));
        let Value::Dict(fields) = *state else { panic!("expected rowset attributes") };
        let field = |key: &str| fields.iter().find(|(k, _)| *k == Value::bytes(key)).unwrap().1.clone();
        assert_eq!(field("header"), Value::List(vec![Value::bytes("languageID"), Value::bytes("languageName"), Value::bytes("translatedLanguageName")]));
        let lines = field("lines");
        assert_eq!(lines.seq().unwrap().len(), 9);
        assert_eq!(lines.seq().unwrap()[0].seq().unwrap()[0], Value::bytes("EN"));
        assert_eq!(lines.seq().unwrap()[5], Value::List(vec![Value::bytes("JA"), Value::text("Japanese"), Value::text("日本語")]));
        let mut request = call("GetLanguages");
        request.args.push(Value::bytes("DE"));
        assert!(matches!(reply(&request), Some(Outcome::UserError { .. })));
        for method in ["DbgGetLanguagePack", "GetMessagesByLanguageID", "GetTextsForGroup"] {
            assert!(matches!(reply(&call(method)), Some(Outcome::UserError { .. })));
        }
    }

    #[test]
    fn language_selection_and_english_fallback() {
        for language in ["EN", "DE", "ES", "FR", "IT", "JA", "KO", "PT", "RU"] {
            assert_eq!(canonical_language(&language.to_ascii_lowercase()), Some(language));
        }
        for invalid in ["FR ", "../../FR", "ZH", "", "FR-FR"] {
            assert_eq!(canonical_language(invalid), None);
            assert_eq!(resolve_language(invalid), "EN");
        }
    }
}
