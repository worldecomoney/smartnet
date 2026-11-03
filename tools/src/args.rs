//! Tiny argv parser for the tools (no clap): `--key value`, `--key=value`,
//! bare `--flags`, and positionals.

use std::collections::HashMap;

#[derive(Default)]
pub struct Args {
    /// First non-flag argument (the subcommand).
    pub cmd: Option<String>,
    /// Everything else that wasn't consumed as an option value.
    pub pos: Vec<String>,
    opts: HashMap<String, String>,
    flags: HashMap<String, bool>,
}

impl Args {
    pub fn parse(argv: &[String]) -> Args {
        let mut out = Args::default();
        let mut i = 0;
        while i < argv.len() {
            let a = &argv[i];
            if let Some(body) = a.strip_prefix("--") {
                let (k, v) = match body.split_once('=') {
                    Some((k, v)) => (k.to_string(), Some(v.to_string())),
                    None => match argv.get(i + 1).filter(|n| !n.starts_with("--")) {
                        Some(n) => {
                            let n = n.clone();
                            i += 1;
                            (body.to_string(), Some(n))
                        }
                        None => (body.to_string(), None),
                    },
                };
                match v {
                    Some(v) => {
                        out.opts.insert(k, v);
                    }
                    None => {
                        out.flags.insert(k, true);
                    }
                }
            } else if out.cmd.is_none() {
                out.cmd = Some(a.clone());
            } else {
                out.pos.push(a.clone());
            }
            i += 1;
        }
        out
    }

    pub fn get(&self, k: &str) -> Option<&str> {
        self.opts.get(k).map(String::as_str)
    }

    pub fn get_or(&self, k: &str, default: &str) -> String {
        self.get(k).map(str::to_string).unwrap_or_else(|| default.to_string())
    }

    pub fn num<T: std::str::FromStr>(&self, k: &str, default: T) -> T {
        match self.get(k) {
            Some(v) => v.parse().unwrap_or(default),
            None => default,
        }
    }

    pub fn flag(&self, k: &str) -> bool {
        self.flags.contains_key(k) || self.get(k).map(|v| v != "false").unwrap_or(false)
    }
}
