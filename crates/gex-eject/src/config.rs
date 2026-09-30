//! `user/eject.json`: the user's settings for a project. Created once, from a
//! template, and never touched by a re-eject.

use crate::json::{self, obj, opt_str, str_field, Json};
use anyhow::{anyhow, bail, Result};
use gex_native::aot::Abi;
use std::path::Path;

pub const CONFIG_FILE: &str = "user/eject.json";
/// Where `build.sh` puts the module it compiles from `user/src/*.c`.
pub const USER_WASM: &str = "build/user.wasm";

/// How much of each object the build translates.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Coverage {
    /// Every block the recorded session ran, with every page they touch made
    /// whole: exactly what was seen. An object the session never ran code in
    /// gets its entry point only.
    Profile,
    /// [`Coverage::Profile`], plus what discovery finds from the symbol table
    /// and the data. Bigger, and it covers paths other inputs take.
    Static,
    /// Every decodable address. Bigger; nothing static can be missed.
    Exhaustive,
}

/// One library replacement: a guest function served by a user wasm function.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Replace {
    /// Which object: its guest path or its file name.
    pub object: String,
    /// The object's content hash the replacement was written against. A
    /// changed object makes the build fail, naming the stale replacement.
    pub sha256: String,
    pub symbol: String,
    /// The wasm module, relative to the project (default [`USER_WASM`]).
    pub wasm: String,
    /// The function it exports (default: `symbol`).
    pub export: String,
    pub abi: Abi,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Config {
    pub replace: Vec<Replace>,
    pub coverage: Coverage,
    /// Arguments after `argv[0]` for a run that gives none (default: the
    /// recorded ones).
    pub args: Option<Vec<String>>,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            replace: Vec::new(),
            coverage: Coverage::Profile,
            args: None,
        }
    }
}

/// The file a new project starts with.
pub fn template() -> String {
    json::write(&obj(vec![
        (
            "about",
            json::s(
                "Your settings; gexc eject never rewrites this file. \"replace\" lists guest \
                 functions served by your wasm (see docs/eject.md), e.g. {\"object\": \"prog\", \
                 \"sha256\": \"sha256:...\", \"symbol\": \"strlen\", \"export\": \"my_strlen\", \
                 \"abi\": \"c\"}. \"coverage\" is \"profile\" (what the session ran), \"static\" (and what discovery finds) or \"exhaustive\". \"args\" \
                 replaces the recorded arguments.",
            ),
        ),
        ("replace", Json::Arr(Vec::new())),
        ("coverage", json::s("profile")),
        ("args", Json::Null),
    ]))
}

impl Config {
    pub fn from_json(text: &str) -> Result<Config> {
        let what = CONFIG_FILE;
        let v = Json::parse(text).map_err(|e| anyhow!("{what}: {e}"))?;
        let Json::Obj(kv) = &v else {
            bail!("{what}: not a JSON object");
        };
        for (k, _) in kv {
            if !matches!(k.as_str(), "about" | "replace" | "coverage" | "args") {
                bail!("{what}: unknown setting \"{k}\"");
            }
        }
        let mut c = Config::default();
        if let Some(Json::Arr(list)) = v.get("replace") {
            for (i, r) in list.iter().enumerate() {
                let w = format!("{what}: replace[{i}]");
                if let Json::Obj(kv) = r {
                    for (k, _) in kv {
                        if !matches!(k.as_str(), "object" | "sha256" | "symbol" | "wasm" | "export" | "abi") {
                            bail!("{w}: unknown field \"{k}\"");
                        }
                    }
                }
                let symbol = str_field(r, "symbol", &w)?;
                let abi = match opt_str(r, "abi") {
                    None => Abi::C,
                    Some(a) => Abi::parse(&a).ok_or_else(|| anyhow!("{w}: abi \"{a}\" is not \"c\" or \"ctx\""))?,
                };
                c.replace.push(Replace {
                    object: str_field(r, "object", &w)?,
                    sha256: str_field(r, "sha256", &w)?,
                    export: opt_str(r, "export").unwrap_or_else(|| symbol.clone()),
                    wasm: opt_str(r, "wasm").unwrap_or_else(|| USER_WASM.to_string()),
                    symbol,
                    abi,
                });
            }
        } else if !matches!(v.get("replace"), None | Some(Json::Null)) {
            bail!("{what}: \"replace\" is not a list");
        }
        c.coverage = match opt_str(&v, "coverage").as_deref() {
            None | Some("profile") => Coverage::Profile,
            Some("static") => Coverage::Static,
            Some("exhaustive") => Coverage::Exhaustive,
            Some(o) => bail!("{what}: coverage \"{o}\" is not \"profile\", \"static\" or \"exhaustive\""),
        };
        c.args = match v.get("args") {
            None | Some(Json::Null) => None,
            Some(_) => Some(json::str_list(&v, "args", what)?),
        };
        Ok(c)
    }

    /// The project's config, or the default when it has none.
    pub fn load(dir: &Path) -> Result<(Config, Vec<u8>)> {
        let p = dir.join(CONFIG_FILE);
        match std::fs::read(&p) {
            Ok(bytes) => {
                let text = String::from_utf8(bytes.clone()).map_err(|_| anyhow!("{CONFIG_FILE} is not UTF-8"))?;
                Ok((Config::from_json(&text)?, bytes))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok((Config::default(), Vec::new())),
            Err(e) => Err(anyhow!("reading {}: {e}", p.display())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_template_is_the_default() {
        assert_eq!(Config::from_json(&template()).unwrap(), Config::default());
    }

    #[test]
    fn reads_a_replacement_and_rejects_typos() {
        let c = Config::from_json(
            r#"{"replace": [{"object": "ack", "sha256": "sha256:ab", "symbol": "ack"}],
                "coverage": "exhaustive", "args": ["3"]}"#,
        )
        .unwrap();
        assert_eq!(c.replace[0].export, "ack");
        assert_eq!(c.replace[0].wasm, USER_WASM);
        assert_eq!(c.replace[0].abi, Abi::C);
        assert_eq!(c.coverage, Coverage::Exhaustive);
        assert_eq!(c.args, Some(vec!["3".to_string()]));
        assert!(Config::from_json(r#"{"replaec": []}"#).is_err());
        assert!(Config::from_json(r#"{"replace": [{"object": "a", "sha256": "x", "symbol": "s", "abii": "c"}]}"#).is_err());
        assert!(Config::from_json(r#"{"coverage": "most"}"#).is_err());
    }
}
