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

/// What a replacement serves.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Target {
    /// The function a symbol names, at its definition: every way of reaching
    /// it (direct call, PLT through the GOT, pointer) runs the replacement.
    Symbol(String),
    /// The function at a link-time address, for an object without symbols.
    /// `bytes` is required with it: an offset means nothing in another build.
    Offset { offset: u64, size: u64 },
    /// An import of `object` (or, with no object, of every object): its PLT
    /// stub for this symbol. Calls from elsewhere still reach the definition.
    Import(String),
    /// A whole library: every function it exports is served by the module's
    /// export of the same name (after `prefix`). One the module lacks fails
    /// loudly, naming itself, when it is called.
    Library { prefix: String },
}

/// One library replacement: a guest function (or a library's) served by a
/// user wasm function.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Replace {
    /// Which object: its guest path, file name or soname. Empty for an
    /// `import` meant for every object.
    pub object: String,
    /// The object's content hash the replacement was written against, when
    /// given: a changed object then fails the build, naming the replacement.
    pub sha256: Option<String>,
    /// `sha256:` of the replaced function's original bytes, when given: a
    /// changed function fails the build the same way.
    pub bytes: Option<String>,
    pub target: Target,
    /// The wasm module, relative to the project (default [`USER_WASM`]).
    pub wasm: String,
    /// The function it exports (default: the symbol). Unused for a library.
    pub export: String,
    pub abi: Abi,
}

/// A stateful export provider (docs/eject-pe.md §5): a Windows library's
/// exports served by a wasm module built from `dir` (its `build.sh`), such
/// as the Lua provider an eject of a LuaJIT program sets up.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Provider {
    /// The DLL, by file name (`lua51.dll`).
    pub library: String,
    /// Its sources, relative to the project, with a `build.sh OUT GEN`.
    pub dir: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Config {
    pub replace: Vec<Replace>,
    /// Windows programs: libraries replaced by stateful providers.
    pub providers: Vec<Provider>,
    /// Windows programs: function starts recovery must take, as
    /// `image.dll+0xOFFSET` (the run names them when it misses one).
    pub seeds: Vec<String>,
    pub coverage: Coverage,
    /// Arguments after `argv[0]` for a run that gives none (default: the
    /// recorded ones).
    pub args: Option<Vec<String>>,
    /// Only the system calls the session made are allowed (default), or all.
    pub all_syscalls: bool,
    /// System calls allowed besides the recorded ones, by name or number.
    pub allow_syscalls: Vec<String>,
    /// Translate code the build never saw instead of failing on it, for
    /// every run of this build (default off; a run can ask with
    /// `--jit-fallback`).
    pub jit_fallback: bool,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            replace: Vec::new(),
            providers: Vec::new(),
            seeds: Vec::new(),
            coverage: Coverage::Profile,
            args: None,
            all_syscalls: false,
            allow_syscalls: Vec::new(),
            jit_fallback: false,
        }
    }
}

/// The file a new project starts with.
pub fn template() -> String {
    template_with(&[])
}

/// [`template`], with stateful providers set up.
pub fn template_with(providers: &[Provider]) -> String {
    let mut fields = template_fields();
    if !providers.is_empty() {
        fields.push((
            "providers",
            Json::Arr(providers.iter().map(|p| obj(vec![("library", json::s(&p.library)), ("dir", json::s(&p.dir))])).collect()),
        ));
    }
    json::write(&obj(fields))
}

fn template_fields() -> Vec<(&'static str, Json)> {
    vec![
        (
            "about",
            json::s(
                "Your settings; gexc eject never rewrites this file. \"replace\" lists guest \
                 functions served by your wasm (see docs/eject.md), e.g. {\"object\": \"prog\", \
                 \"symbol\": \"strlen\", \"export\": \"my_strlen\", \"abi\": \"c\"}, or by \"offset\", \
                 \"import\" or \"library\"; `gexc eject override` prints a checked entry. \"coverage\" is \"profile\" (what the session ran), \"static\" (and what discovery finds) or \"exhaustive\". \"args\" \
                 replaces the recorded arguments. \"syscalls\": \"recorded\" allows only the system \
                 calls the session made (\"all\": any); \"allow_syscalls\" adds some by name. \"jit_fallback\": true translates code the build \
                 never saw instead of failing on it.",
            ),
        ),
        ("replace", Json::Arr(Vec::new())),
        ("coverage", json::s("profile")),
        ("args", Json::Null),
        ("syscalls", json::s("recorded")),
        ("allow_syscalls", Json::Arr(Vec::new())),
        ("jit_fallback", json::b(false)),
    ]
}

impl Config {
    pub fn from_json(text: &str) -> Result<Config> {
        let what = CONFIG_FILE;
        let v = Json::parse(text).map_err(|e| anyhow!("{what}: {e}"))?;
        let Json::Obj(kv) = &v else {
            bail!("{what}: not a JSON object");
        };
        for (k, _) in kv {
            if !matches!(k.as_str(), "about" | "replace" | "providers" | "seeds" | "coverage" | "args" | "syscalls" | "allow_syscalls" | "jit_fallback") {
                bail!("{what}: unknown setting \"{k}\"");
            }
        }
        let mut c = Config::default();
        if let Some(Json::Arr(list)) = v.get("replace") {
            for (i, r) in list.iter().enumerate() {
                let w = format!("{what}: replace[{i}]");
                if let Json::Obj(kv) = r {
                    for (k, _) in kv {
                        if !matches!(
                            k.as_str(),
                            "object" | "sha256" | "bytes" | "symbol" | "offset" | "size" | "import"
                                | "library" | "prefix" | "wasm" | "export" | "abi"
                        ) {
                            bail!("{w}: unknown field \"{k}\"");
                        }
                    }
                }
                let hex = |k: &str| -> Result<Option<u64>> {
                    match r.get(k) {
                        None | Some(Json::Null) => Ok(None),
                        Some(Json::Num(n)) => n.parse().map(Some).map_err(|_| anyhow!("{w}: bad {k}")),
                        Some(Json::Str(t)) => u64::from_str_radix(t.trim_start_matches("0x"), 16)
                            .map(Some)
                            .map_err(|_| anyhow!("{w}: {k} {t:?} is not a hex number")),
                        Some(_) => bail!("{w}: {k} is not a number"),
                    }
                };
                let picked: Vec<&str> = ["symbol", "offset", "import", "library"]
                    .into_iter()
                    .filter(|k| r.get(k).is_some())
                    .collect();
                let bytes = opt_str(r, "bytes");
                let target = match picked.as_slice() {
                    ["symbol"] => Target::Symbol(str_field(r, "symbol", &w)?),
                    ["offset"] => {
                        if bytes.is_none() {
                            bail!("{w}: an \"offset\" needs \"bytes\", the hash of the function it names (gexc eject override prints both)");
                        }
                        Target::Offset {
                            offset: hex("offset")?.unwrap(),
                            size: hex("size")?.ok_or_else(|| anyhow!("{w}: an \"offset\" needs a \"size\""))?,
                        }
                    }
                    ["import"] => Target::Import(str_field(r, "import", &w)?),
                    ["library"] => Target::Library { prefix: opt_str(r, "prefix").unwrap_or_default() },
                    [] => bail!("{w}: say what it replaces: \"symbol\", \"offset\", \"import\" or \"library\""),
                    _ => bail!("{w}: give one of {}", picked.join(", ")),
                };
                let abi = match opt_str(r, "abi") {
                    None => Abi::C,
                    Some(a) => Abi::parse(&a).ok_or_else(|| anyhow!("{w}: abi \"{a}\" is not \"c\", \"ctx\" or \"entry\""))?,
                };
                let object = match &target {
                    Target::Library { .. } => str_field(r, "library", &w)?,
                    Target::Import(_) => opt_str(r, "object").unwrap_or_default(),
                    _ => str_field(r, "object", &w)?,
                };
                let export = match (&target, opt_str(r, "export")) {
                    (_, Some(e)) => e,
                    (Target::Symbol(n) | Target::Import(n), None) => n.clone(),
                    (Target::Offset { offset, .. }, None) => format!("fn_{offset:x}"),
                    (Target::Library { .. }, None) => String::new(),
                };
                c.replace.push(Replace {
                    object,
                    sha256: opt_str(r, "sha256"),
                    bytes,
                    target,
                    export,
                    wasm: opt_str(r, "wasm").unwrap_or_else(|| USER_WASM.to_string()),
                    abi,
                });
            }
        } else if !matches!(v.get("replace"), None | Some(Json::Null)) {
            bail!("{what}: \"replace\" is not a list");
        }
        if let Some(Json::Arr(list)) = v.get("providers") {
            for (i, r) in list.iter().enumerate() {
                let w = format!("{what}: providers[{i}]");
                c.providers.push(Provider { library: str_field(r, "library", &w)?, dir: str_field(r, "dir", &w)? });
            }
        } else if !matches!(v.get("providers"), None | Some(Json::Null)) {
            bail!("{what}: \"providers\" is not a list");
        }
        c.coverage = match opt_str(&v, "coverage").as_deref() {
            None | Some("profile") => Coverage::Profile,
            Some("static") => Coverage::Static,
            Some("exhaustive") => Coverage::Exhaustive,
            Some(o) => bail!("{what}: coverage \"{o}\" is not \"profile\", \"static\" or \"exhaustive\""),
        };
        c.all_syscalls = match opt_str(&v, "syscalls").as_deref() {
            None | Some("recorded") => false,
            Some("all") => true,
            Some(o) => bail!("{what}: syscalls \"{o}\" is not \"recorded\" or \"all\""),
        };
        c.allow_syscalls = json::str_list(&v, "allow_syscalls", what)?;
        c.seeds = json::str_list(&v, "seeds", what)?;
        c.jit_fallback = match v.get("jit_fallback") {
            None | Some(Json::Null) => false,
            Some(Json::Bool(b)) => *b,
            Some(_) => bail!("{what}: jit_fallback is not true or false"),
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
        assert_eq!(c.replace[0].target, Target::Symbol("ack".into()));
        assert_eq!(c.replace[0].sha256.as_deref(), Some("sha256:ab"));
        assert_eq!(c.replace[0].wasm, USER_WASM);
        assert_eq!(c.replace[0].abi, Abi::C);
        assert_eq!(c.coverage, Coverage::Exhaustive);
        assert_eq!(c.args, Some(vec!["3".to_string()]));
        assert!(Config::from_json(r#"{"replaec": []}"#).is_err());
        assert!(Config::from_json(r#"{"replace": [{"object": "a", "sha256": "x", "symbol": "s", "abii": "c"}]}"#).is_err());
        assert!(Config::from_json(r#"{"coverage": "most"}"#).is_err());
        // An offset means nothing without the bytes it named.
        assert!(Config::from_json(r#"{"replace": [{"object": "a", "offset": "0x10", "size": 4}]}"#).is_err());
        let c = Config::from_json(
            r#"{"replace": [{"object": "a", "offset": "0x10", "size": "0x20", "bytes": "sha256:cd"},
                            {"import": "getenv"},
                            {"library": "libSDL2.so", "prefix": "sdl_", "wasm": "user/sdl.wasm"}]}"#,
        )
        .unwrap();
        assert_eq!(c.replace[0].target, Target::Offset { offset: 0x10, size: 0x20 });
        assert_eq!(c.replace[0].export, "fn_10");
        assert_eq!(c.replace[1].object, "");
        assert_eq!(c.replace[2].target, Target::Library { prefix: "sdl_".into() });
        assert_eq!(c.replace[2].object, "libSDL2.so");
        // Two targets at once is ambiguous.
        assert!(Config::from_json(r#"{"replace": [{"object": "a", "symbol": "f", "import": "f"}]}"#).is_err());
    }
}
