//! JSON in and out, without a serialisation dependency: parsing is
//! `gex_xlate::profile::Json`'s, and this writes the same value type,
//! pretty-printed with keys in the order given, so a project's files are
//! byte-for-byte reproducible.

pub use gex_xlate::profile::Json;
use gex_xlate::profile::jstr;

pub fn s(v: impl AsRef<str>) -> Json {
    Json::Str(v.as_ref().to_string())
}
pub fn n(v: u64) -> Json {
    Json::Num(v.to_string())
}
pub fn b(v: bool) -> Json {
    Json::Bool(v)
}
pub fn opt(v: Option<impl AsRef<str>>) -> Json {
    v.map_or(Json::Null, s)
}
pub fn strs<I: IntoIterator<Item = S>, S: AsRef<str>>(v: I) -> Json {
    Json::Arr(v.into_iter().map(s).collect())
}
pub fn obj(kv: Vec<(&str, Json)>) -> Json {
    Json::Obj(kv.into_iter().map(|(k, v)| (k.to_string(), v)).collect())
}

/// Pretty-printed, two-space indent, trailing newline. Short arrays of
/// scalars stay on one line.
pub fn write(v: &Json) -> String {
    let mut out = String::new();
    emit(v, 0, &mut out);
    out.push('\n');
    out
}

fn scalar(v: &Json) -> Option<String> {
    Some(match v {
        Json::Null => "null".into(),
        Json::Bool(b) => b.to_string(),
        Json::Num(n) => n.clone(),
        Json::Str(s) => jstr(s),
        _ => return None,
    })
}

fn emit(v: &Json, depth: usize, out: &mut String) {
    let pad = |d: usize| "  ".repeat(d);
    if let Some(t) = scalar(v) {
        out.push_str(&t);
        return;
    }
    match v {
        Json::Arr(a) if a.is_empty() => out.push_str("[]"),
        Json::Obj(o) if o.is_empty() => out.push_str("{}"),
        Json::Arr(a) => {
            let flat: Option<Vec<String>> = a.iter().map(scalar).collect();
            if let Some(items) = flat {
                let line = items.join(", ");
                if line.len() + depth * 2 < 100 {
                    out.push('[');
                    out.push_str(&line);
                    out.push(']');
                    return;
                }
            }
            out.push_str("[\n");
            for (i, x) in a.iter().enumerate() {
                out.push_str(&pad(depth + 1));
                emit(x, depth + 1, out);
                if i + 1 < a.len() {
                    out.push(',');
                }
                out.push('\n');
            }
            out.push_str(&pad(depth));
            out.push(']');
        }
        Json::Obj(o) => {
            out.push_str("{\n");
            for (i, (k, x)) in o.iter().enumerate() {
                out.push_str(&pad(depth + 1));
                out.push_str(&jstr(k));
                out.push_str(": ");
                emit(x, depth + 1, out);
                if i + 1 < o.len() {
                    out.push(',');
                }
                out.push('\n');
            }
            out.push_str(&pad(depth));
            out.push('}');
        }
        _ => unreachable!(),
    }
}

/// Field access that says what was missing.
pub fn field<'a>(v: &'a Json, k: &str, what: &str) -> anyhow::Result<&'a Json> {
    v.get(k).ok_or_else(|| anyhow::anyhow!("{what}: no \"{k}\""))
}
pub fn str_field(v: &Json, k: &str, what: &str) -> anyhow::Result<String> {
    field(v, k, what)?
        .as_str()
        .map(str::to_string)
        .ok_or_else(|| anyhow::anyhow!("{what}: \"{k}\" is not a string"))
}
pub fn opt_str(v: &Json, k: &str) -> Option<String> {
    v.get(k).and_then(|x| x.as_str()).map(str::to_string)
}
pub fn str_list(v: &Json, k: &str, what: &str) -> anyhow::Result<Vec<String>> {
    match v.get(k) {
        None | Some(Json::Null) => Ok(Vec::new()),
        Some(Json::Arr(a)) => a
            .iter()
            .map(|x| {
                x.as_str()
                    .map(str::to_string)
                    .ok_or_else(|| anyhow::anyhow!("{what}: \"{k}\" holds a non-string"))
            })
            .collect(),
        Some(_) => anyhow::bail!("{what}: \"{k}\" is not a list"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_and_is_stable() {
        let v = obj(vec![
            ("a", s("x\"y")),
            ("b", strs(["1", "2"])),
            ("c", Json::Arr(vec![obj(vec![("n", n(7)), ("t", b(true))])])),
            ("d", Json::Null),
        ]);
        let text = write(&v);
        assert_eq!(Json::parse(&text).unwrap(), v);
        assert_eq!(write(&Json::parse(&text).unwrap()), text);
    }
}
