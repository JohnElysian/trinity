//! Original Trinity 45017 language resources. Ordinary clients load translations
//! locally (mlsUtil.LoadTranslations_); this service advertises available packs
//! and implements the developer reload call with the original compressed bytes.

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::path::Path;
use std::sync::OnceLock;

use eve_marshal::Value;
use serde::Deserialize;
use sha2::{Digest, Sha256};

use crate::services::{Call, Ctx, Outcome};
use crate::svc::rows::rowset;

const MANIFEST: &str = include_str!("../../../../data/localization-45017/manifest.json");
const ORIGINAL: [(&str, &[u8]); 8] = [
    ("DE", include_bytes!("../../../../data/localization-45017/translations.de.pickle.zlib")),
    ("ES", include_bytes!("../../../../data/localization-45017/translations.es.pickle.zlib")),
    ("FR", include_bytes!("../../../../data/localization-45017/translations.fr.pickle.zlib")),
    ("IT", include_bytes!("../../../../data/localization-45017/translations.it.pickle.zlib")),
    ("JA", include_bytes!("../../../../data/localization-45017/translations.ja.pickle.zlib")),
    ("KO", include_bytes!("../../../../data/localization-45017/translations.ko.pickle.zlib")),
    ("PT", include_bytes!("../../../../data/localization-45017/translations.pt.pickle.zlib")),
    ("RU", include_bytes!("../../../../data/localization-45017/translations.ru.pickle.zlib")),
];

#[derive(Deserialize)]
struct Manifest {
    build: u32,
    version: u32,
    languages: Vec<PackMetadata>,
}

#[derive(Deserialize)]
struct PackMetadata {
    language_id: String,
    language_name: String,
    translated_language_name: String,
    file: String,
    length: usize,
    sha256: String,
}

struct Pack {
    metadata: PackMetadata,
    compressed: Vec<u8>,
}

struct Resources {
    packs: BTreeMap<&'static str, Pack>,
    english: Vec<u8>,
}

impl Resources {
    fn load(directory: Option<&Path>) -> Self {
        let manifest: Manifest = serde_json::from_str(MANIFEST).expect("bundled language manifest");
        assert_eq!((manifest.build, manifest.version), (45017, 1));
        let mut packs = BTreeMap::new();
        for metadata in manifest.languages {
            let Some(&(id, original)) = ORIGINAL.iter().find(|(id, _)| *id == metadata.language_id) else {
                continue;
            };
            let loaded = match directory {
                Some(directory) => read_pack(&directory.join(&metadata.file), metadata.length),
                None => Ok(original.to_vec()),
            }.and_then(|raw| verify_pack(&metadata, raw));
            match loaded {
                Ok(compressed) => { packs.insert(id, Pack { metadata, compressed }); }
                Err(error) => tracing::warn!(language = id, %error, "language pack unavailable"),
            }
        }
        // EN has no original archive: the client's EN branch returns {}. This
        // literal EMPTY_DICT/STOP is the same value for its debug file loader.
        let mut encoder = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
        encoder.write_all(b"}.").expect("compress English dictionary");
        Self { packs, english: encoder.finish().expect("finish English dictionary") }
    }

    fn canonical(&self, language: &str) -> Option<&'static str> {
        if language.eq_ignore_ascii_case("EN") { return Some("EN"); }
        self.packs.keys().copied().find(|id| language.eq_ignore_ascii_case(id))
    }

    fn languages(&self) -> Value {
        let mut lines = vec![Value::List(vec![Value::bytes("EN"), Value::text("English"), Value::text("English")])];
        lines.extend(self.packs.iter().map(|(id, pack)| Value::List(vec![
            Value::bytes(*id), Value::text(&pack.metadata.language_name),
            Value::text(&pack.metadata.translated_language_name),
        ])));
        // gameui.GetLanguages reads row.languageID (not row[0]).
        rowset(&["languageID", "languageName", "translatedLanguageName"], lines)
    }

    fn debug_pack(&self, language: &str) -> Outcome {
        let Some(language) = self.canonical(language) else {
            return Outcome::custom("The requested language pack is unavailable.");
        };
        let bytes = if language == "EN" { &self.english } else { &self.packs[language].compressed };
        // Original NetClient.dll receiver RVA2471 rejects a frame >1,000,000.
        // The already-compressed DE archive is 4,345,307 bytes, and the original
        // DbgReloadLanguage has no chunk/filter contract. Never disconnect the
        // caller with an oversized frame or install a truncated dictionary.
        // Every other original pack is <153 KB, leaving ample envelope space.
        if bytes.len() > 900_000 {
            return Outcome::custom(format!(
                "The complete {language} debug language pack is {} bytes and cannot fit the original client's 1 MB network limit. Normal localization uses the complete installed resLangPack.{language}.stuff resource.", bytes.len()));
        }
        Outcome::Value(Value::Bytes(bytes.clone()))
    }
}

fn read_pack(path: &Path, expected_length: usize) -> Result<Vec<u8>, String> {
    let file = std::fs::File::open(path).map_err(|error| error.to_string())?;
    let mut raw = Vec::new();
    file.take(expected_length as u64 + 1).read_to_end(&mut raw).map_err(|error| error.to_string())?;
    Ok(raw)
}

fn verify_pack(metadata: &PackMetadata, raw: Vec<u8>) -> Result<Vec<u8>, String> {
    if raw.len() != metadata.length { return Err("original resource length mismatch".into()); }
    let hash: String = Sha256::digest(&raw).iter().map(|byte| format!("{byte:02x}")).collect();
    if hash != metadata.sha256 { return Err("original resource SHA-256 mismatch".into()); }
    Ok(raw)
}

fn resources() -> &'static Resources {
    static RESOURCES: OnceLock<Resources> = OnceLock::new();
    RESOURCES.get_or_init(|| {
        let directory = std::env::var_os("RMR_LOCALIZATION_DIR").map(std::path::PathBuf::from);
        Resources::load(directory.as_deref())
    })
}

/// An actually loaded pack, normalized to its canonical client language ID.
/// Unknown IDs, paths and missing/corrupt external resources are rejected.
pub fn canonical_language(language: &str) -> Option<&'static str> {
    resources().canonical(language)
}

/// The client falls back to its implicit English text for unsupported languages.
pub fn resolve_language(language: &str) -> &'static str {
    canonical_language(language).unwrap_or("EN")
}

pub async fn language_svc(call: &Call, ctx: &Ctx<'_>) -> Option<Outcome> {
    reply(resources(), call, ctx.session.language_id.as_deref())
}

fn reply(resources: &Resources, call: &Call, language: Option<&str>) -> Option<Outcome> {
    Some(match call.method.as_str() {
        "GetLanguages" if call.args.is_empty() && call.kwargs.is_empty() => Outcome::Value(resources.languages()),
        "DbgGetLanguagePack" if call.args.is_empty() && call.kwargs.is_empty() =>
            resources.debug_pack(language.unwrap_or("EN")),
        "GetLanguages" | "DbgGetLanguagePack" => Outcome::custom("This language method takes no arguments."),
        // These legacy stubs have no calls in the 45017 source/disassembly. The
        // original client reads messagePackage and labels from its resources;
        // group membership and the older RPC contracts are not recovered.
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

    fn value(outcome: Option<Outcome>) -> Value {
        match outcome { Some(Outcome::Value(value)) => value, _ => panic!("expected successful language response") }
    }

    #[test]
    fn original_language_rpc_shape_and_no_argument_session_selection() {
        let resources = Resources::load(None);
        let languages = value(reply(&resources, &call("GetLanguages"), Some("FR")));
        let Value::Instance { class, state } = languages else { panic!("expected rowset") };
        assert_eq!(*class, Value::bytes("util.Rowset"));
        let Value::Dict(fields) = *state else { panic!("expected rowset attributes") };
        let field = |key: &str| fields.iter().find(|(k, _)| *k == Value::bytes(key)).unwrap().1.clone();
        assert_eq!(field("header"), Value::List(vec![Value::bytes("languageID"), Value::bytes("languageName"), Value::bytes("translatedLanguageName")]));
        let lines = field("lines");
        assert_eq!(lines.seq().unwrap().len(), 9);
        assert_eq!(lines.seq().unwrap()[0].seq().unwrap()[0], Value::bytes("EN"));
        assert_eq!(value(reply(&resources, &call("DbgGetLanguagePack"), Some("FR"))), Value::Bytes(resources.packs["FR"].compressed.clone()));
        let mut request = call("DbgGetLanguagePack");
        request.args.push(Value::bytes("DE"));
        assert!(matches!(reply(&resources, &request, Some("FR")), Some(Outcome::UserError { .. })));
        for method in ["GetMessagesByLanguageID", "GetTextsForGroup"] {
            assert!(matches!(reply(&resources, &call(method), Some("EN")), Some(Outcome::UserError { .. })));
        }
    }

    #[test]
    fn complete_original_debug_blobs_fit_actual_sealed_response_or_fail_explicitly() {
        let resources = Resources::load(None);
        let table = eve_marshal::StringTable::default();
        let request = eve_proto::MachoPacket::call_req(Value::None);
        for (language, original) in ORIGINAL {
            let outcome = reply(&resources, &call("DbgGetLanguagePack"), Some(language));
            if language == "DE" {
                assert_eq!(resources.packs[language].compressed, original);
                assert_eq!(original.len(), 4_345_307);
                assert!(matches!(outcome, Some(Outcome::UserError { .. })));
                // Recompressing the compressed source does not evade the native
                // limit; no chunk reassembly exists in DbgReloadLanguage.
                let raw = eve_marshal::save(&Value::Bytes(original.to_vec()), &table).unwrap();
                let compressed = eve_proto::maybe_compress(&raw, 0);
                assert!(compressed.len() > eve_proto::frame::MAX_FRAME_LEN as usize);
                continue;
            }
            let result = value(outcome);
            assert_eq!(result, Value::Bytes(original.to_vec()));
            let response = crate::services::make_response(&request, &result, &table).unwrap();
            let raw = eve_marshal::save(&response.to_value().unwrap(), &table).unwrap();
            let sealed = eve_proto::handshake::seal_bytes(&eve_proto::maybe_compress(&raw, 0), &table).unwrap();
            let framed = eve_proto::encode_frame(&sealed).unwrap();
            assert_eq!(u32::from_le_bytes(framed[..4].try_into().unwrap()) as usize, sealed.len());
            let plain = eve_proto::decompress(&eve_proto::handshake::unseal(&framed[4..], &table)).unwrap();
            let decoded = eve_marshal::load(&plain, &table).unwrap();
            assert_eq!(decoded, response.to_value().unwrap());
        }
        let english = value(reply(&resources, &call("DbgGetLanguagePack"), None));
        let mut literal = Vec::new();
        flate2::read::ZlibDecoder::new(english.as_bytes().unwrap()).read_to_end(&mut literal).unwrap();
        assert_eq!(literal, b"}.");
    }

    #[test]
    fn unavailable_or_corrupt_override_is_not_advertised_or_accepted() {
        let path = std::env::temp_dir().join(format!("trinity-language-{}-{}", std::process::id(), crate::bulkdata::filetime_now()));
        std::fs::create_dir(&path).unwrap();
        let (_, original) = ORIGINAL.iter().find(|(id, _)| *id == "FR").unwrap();
        std::fs::write(path.join("translations.fr.pickle.zlib"), original).unwrap();
        let mut corrupt = original.to_vec();
        corrupt[20] ^= 1;
        std::fs::write(path.join("translations.es.pickle.zlib"), &corrupt).unwrap();
        let resources = Resources::load(Some(&path));
        assert_eq!(resources.packs.keys().copied().collect::<Vec<_>>(), ["FR"]);
        assert_eq!(resources.canonical("fr"), Some("FR"));
        assert_eq!(resources.canonical("en"), Some("EN"));
        for invalid in ["FR ", "../../FR", "ZH", "ES", "", "FR-FR"] {
            assert_eq!(resources.canonical(invalid), None);
        }
        let metadata = &resources.packs["FR"].metadata;
        assert!(verify_pack(metadata, corrupt).unwrap_err().contains("SHA-256"));
        assert!(verify_pack(metadata, original[..original.len()-1].to_vec()).unwrap_err().contains("length"));
        assert_eq!(resolve_language("not-a-language"), "EN");
        std::fs::remove_file(path.join("translations.fr.pickle.zlib")).unwrap();
        std::fs::remove_file(path.join("translations.es.pickle.zlib")).unwrap();
        std::fs::remove_dir(path).unwrap();
    }
}
