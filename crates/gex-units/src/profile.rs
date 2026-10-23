//! A profile of a build's units: for each slot an instrumented run entered,
//! when it was first entered and how often, and the run's marks.
//!
//! An instrumented build (`"cold": "instrument"`, gex.mjs `covFlush`) says
//! `[gex-prof] KEY SLOT:FIRST:CALLS,...` on the console: FIRST is the slot's
//! first entry in ms of the epoch, said once, and CALLS the entries since the
//! slot's last report. The driver says `[gex-mark] NAME MS` (ms of the
//! epoch) for the moments start-up is measured to: `start` (the page's
//! navigation) and then `desktop`, `explorer`, and so on. Times in the
//! profile are ms from `start`.
//!
//! Its text form, one record a line (tab-separated), is what
//! `gexc units profile` writes and [`Profile::parse`] reads:
//!
//! ```text
//! mark  NAME  MS
//! slot  KEY   SLOT  FIRST_MS  CALLS
//! ```
//!
//! Several runs add up: a slot's first use is its earliest, its calls the
//! sum over the runs, and each mark its latest.
//!
//! A profile names functions of the images by slot only. It is a build
//! product, never committed (an image may be one whose code is not ours).
use anyhow::{anyhow, bail, Result};
use std::collections::BTreeMap;

/// One slot's use.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Use {
    /// First entry, ms from the run's start.
    pub first_ms: f64,
    pub calls: u64,
}

#[derive(Default, Debug)]
pub struct Profile {
    /// Mark name -> ms from the start.
    pub marks: BTreeMap<String, f64>,
    /// Unit key -> slot -> use.
    pub units: BTreeMap<String, BTreeMap<u32, Use>>,
}

impl Profile {
    /// Add the run a console log holds. A log without a `start` mark has its
    /// earliest first use as the start.
    pub fn add_log(&mut self, log: &str) -> Result<usize> {
        let mut marks: Vec<(String, f64)> = Vec::new();
        // key -> slot -> (first epoch ms, calls)
        let mut raw: BTreeMap<String, BTreeMap<u32, (Option<f64>, u64)>> = BTreeMap::new();
        let mut lines = 0;
        for line in log.lines() {
            if let Some(rest) = line.split("[gex-mark] ").nth(1) {
                let mut it = rest.split_whitespace();
                if let (Some(n), Some(t)) = (it.next(), it.next()) {
                    if let Ok(t) = t.parse::<f64>() {
                        marks.push((n.to_string(), t));
                    }
                }
                continue;
            }
            let Some(rest) = line.split("[gex-prof] ").nth(1) else { continue };
            let mut it = rest.split_whitespace();
            let (Some(key), Some(list)) = (it.next(), it.next()) else { continue };
            lines += 1;
            let m = raw.entry(key.to_string()).or_default();
            for e in list.split(',').filter(|e| !e.is_empty()) {
                let mut f = e.split(':');
                let (Some(s), Some(t), Some(c)) = (f.next(), f.next(), f.next()) else {
                    bail!("[gex-prof] {key}: bad entry {e:?}");
                };
                let s: u32 = s.parse().map_err(|_| anyhow!("[gex-prof] {key}: bad slot in {e:?}"))?;
                let c: u64 = c.parse().map_err(|_| anyhow!("[gex-prof] {key}: bad count in {e:?}"))?;
                let x = m.entry(s).or_insert((None, 0));
                if !t.is_empty() {
                    let t: f64 = t.parse().map_err(|_| anyhow!("[gex-prof] {key}: bad time in {e:?}"))?;
                    x.0 = Some(x.0.map_or(t, |o: f64| o.min(t)));
                }
                x.1 += c;
            }
        }
        if lines == 0 {
            return Ok(0);
        }
        let start = marks.iter().find(|(n, _)| n == "start").map(|(_, t)| *t).unwrap_or_else(|| {
            raw.values().flat_map(|m| m.values()).filter_map(|x| x.0).fold(f64::INFINITY, f64::min)
        });
        for (n, t) in marks {
            if n == "start" {
                continue;
            }
            let t = t - start;
            let e = self.marks.entry(n).or_insert(t);
            *e = e.max(t);
        }
        for (key, m) in raw {
            let u = self.units.entry(key).or_default();
            for (s, (t, c)) in m {
                // A slot whose first entry fell in an earlier flush of a
                // Worker this log missed has no time: the end of the run.
                let t = t.map_or(f64::INFINITY, |t| t - start);
                let e = u.entry(s).or_insert(Use { first_ms: t, calls: 0 });
                e.first_ms = e.first_ms.min(t);
                e.calls += c;
            }
        }
        Ok(lines)
    }

    /// The text form.
    pub fn write(&self) -> String {
        let mut o = String::new();
        for (n, t) in &self.marks {
            o.push_str(&format!("mark\t{n}\t{t:.0}\n"));
        }
        for (k, m) in &self.units {
            for (s, u) in m {
                let t = if u.first_ms.is_finite() { format!("{:.0}", u.first_ms) } else { "inf".to_string() };
                o.push_str(&format!("slot\t{k}\t{s}\t{t}\t{}\n", u.calls));
            }
        }
        o
    }

    /// Read the text form, adding to this profile.
    pub fn parse_into(&mut self, text: &str) -> Result<()> {
        for (i, line) in text.lines().enumerate() {
            let f: Vec<&str> = line.split('\t').collect();
            let bad = || anyhow!("profile line {}: {line:?}", i + 1);
            match f.as_slice() {
                ["mark", n, t] => {
                    let t: f64 = t.parse().map_err(|_| bad())?;
                    let e = self.marks.entry(n.to_string()).or_insert(t);
                    *e = e.max(t);
                }
                ["slot", k, s, t, c] => {
                    let s: u32 = s.parse().map_err(|_| bad())?;
                    let t: f64 = if *t == "inf" { f64::INFINITY } else { t.parse().map_err(|_| bad())? };
                    let c: u64 = c.parse().map_err(|_| bad())?;
                    let e = self.units.entry(k.to_string()).or_default().entry(s).or_insert(Use { first_ms: t, calls: 0 });
                    e.first_ms = e.first_ms.min(t);
                    e.calls += c;
                }
                [""] => {}
                _ => return Err(bad()),
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn adds_up_runs() {
        let mut p = Profile::default();
        let log = "x [gex-mark] start 1000\n\
                   w [gex-prof] k1 3:1500:2,4::1\n\
                   w [gex-prof] k1 3::5,9:3000:1\n\
                   x [gex-mark] desktop 4000\n";
        assert_eq!(p.add_log(log).unwrap(), 2);
        let u = &p.units["k1"];
        assert_eq!(u[&3], Use { first_ms: 500.0, calls: 7 });
        assert!(u[&4].first_ms.is_infinite());
        assert_eq!(u[&9], Use { first_ms: 2000.0, calls: 1 });
        assert_eq!(p.marks["desktop"], 3000.0);
        let mut q = Profile::default();
        q.parse_into(&p.write()).unwrap();
        q.parse_into(&p.write()).unwrap();
        assert_eq!(q.units["k1"][&3], Use { first_ms: 500.0, calls: 14 });
    }
}
