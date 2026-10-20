//! `commands` of compost-server.yaml and compost.yaml: named commands that
//! compost runs in a container, with arguments checked before anything runs
//! (`compost server cmd`, `compost app cmd`).
//!
//! `command` is a list of words, never a shell string: `{arg}` placeholders
//! are replaced inside words and can't add or split words. Secrets only reach
//! the command through `env` (`{secret:<key>}`), passed to the container as
//! environment variables.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::net::IpAddr;

use anyhow::{Result, bail, ensure};
use serde::{Deserialize, Serialize};
use serde_yaml_ng::Value;

use super::app::env_value;
use super::check_name;
use crate::connector::shell_quote;
use crate::error::config_err;

/// Options of `server cmd` and `app cmd` after the command's name; no
/// argument can have these names.
const RESERVED: &[&str] = &["yes", "dry-run", "help"];

/// One entry of `commands`.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Template {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// compost-server.yaml: the shared app whose container runs the command.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shared_app: Option<String>,
    /// compost-server.yaml: another service of the shared app, e.g. `crowdsec`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub service: Option<String>,
    /// compost.yaml: the compose service whose running container runs it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exec: Option<String>,
    /// compost.yaml: the compose service of a new one-off container.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run: Option<String>,
    #[serde(default, skip_serializing_if = "Args::is_empty")]
    pub args: Args,
    /// Environment variables of the command; values may use `{arg}` and,
    /// on a server, `{secret:<key>}`.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub env: BTreeMap<String, Value>,
    pub command: Vec<String>,
    /// Ask before running, unless `--yes`.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub confirm: bool,
}

/// `args`, in the order they are given: positional values fill them in this order.
#[derive(Debug, Clone, Default)]
pub struct Args(pub Vec<(String, Arg)>);

impl Args {
    fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl<'de> Deserialize<'de> for Args {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        use serde::de::Error;
        let mut args = Vec::new();
        for (name, arg) in serde_yaml_ng::Mapping::deserialize(deserializer)? {
            let Value::String(name) = name else {
                return Err(D::Error::custom("argument names must be strings"));
            };
            let arg = serde_yaml_ng::from_value(arg)
                .map_err(|e| D::Error::custom(format!("{name}: {e}")))?;
            args.push((name, arg));
        }
        Ok(Args(args))
    }
}

impl Serialize for Args {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_map(self.0.iter().map(|(name, arg)| (name, arg)))
    }
}

/// One entry of `args`.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Arg {
    #[serde(rename = "type", default)]
    pub kind: ArgType,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// `string`: a regular expression the whole value must match.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pattern: Option<String>,
    /// `enum`: the allowed values.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub values: Vec<String>,
    /// Makes the argument optional.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default: Option<Value>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ArgType {
    #[default]
    String,
    Int,
    Ip,
    Cidr,
    Duration,
    Email,
    Enum,
}

impl ArgType {
    fn name(self) -> &'static str {
        match self {
            ArgType::String => "string",
            ArgType::Int => "int",
            ArgType::Ip => "ip",
            ArgType::Cidr => "cidr",
            ArgType::Duration => "duration",
            ArgType::Email => "email",
            ArgType::Enum => "enum",
        }
    }
}

impl Arg {
    fn default_value(&self) -> Option<String> {
        self.default.as_ref().and_then(env_value)
    }

    /// Check a value given on the command line (or the default).
    fn check(&self, value: &str) -> Result<()> {
        ensure!(
            !value.chars().any(char::is_control),
            "must not contain control characters such as line breaks"
        );
        match self.kind {
            ArgType::String => match &self.pattern {
                Some(pattern) => {
                    ensure!(anchored(pattern)?.is_match(value), "must match `{pattern}`")
                }
                // Not an option of the tool it is passed to.
                None => ensure!(!value.starts_with('-'), "must not start with `-`"),
            },
            ArgType::Int => ensure!(value.parse::<i64>().is_ok(), "must be a whole number"),
            ArgType::Ip => ensure!(
                value.parse::<IpAddr>().is_ok(),
                "must be an IPv4 or IPv6 address"
            ),
            ArgType::Cidr => ensure!(
                is_cidr(value),
                "must be an address range like 203.0.113.0/24"
            ),
            ArgType::Duration => ensure!(
                humantime::parse_duration(value).is_ok(),
                "must be a duration like 30s, 10m or 4h"
            ),
            ArgType::Email => ensure!(is_email(value), "must be an email address"),
            ArgType::Enum => ensure!(
                self.values.iter().any(|v| v == value),
                "must be one of {}",
                self.values.join(", ")
            ),
        }
        Ok(())
    }
}

fn anchored(pattern: &str) -> Result<regex::Regex> {
    regex::Regex::new(&format!("^(?:{pattern})$"))
        .map_err(|e| anyhow::anyhow!("invalid pattern `{pattern}`: {e}"))
}

fn is_cidr(value: &str) -> bool {
    let Some((address, bits)) = value.split_once('/') else {
        return false;
    };
    let max = match address.parse::<IpAddr>() {
        Ok(IpAddr::V4(_)) => 32,
        Ok(IpAddr::V6(_)) => 128,
        Err(_) => return false,
    };
    bits.parse::<u8>().is_ok_and(|bits| bits <= max)
}

fn is_email(value: &str) -> bool {
    let Some((local, domain)) = value.split_once('@') else {
        return false;
    };
    !local.is_empty()
        && !local.starts_with('-')
        && domain.contains('.')
        && !domain.starts_with('.')
        && !domain.ends_with('.')
        && !value.contains(char::is_whitespace)
        && !domain.contains('@')
}

/// Part of a `command` word or `env` value.
#[derive(Debug, PartialEq)]
enum Token<'a> {
    Text(&'a str),
    /// `{name}`; only replaced when `name` is an argument, else kept as text.
    Arg(&'a str),
    /// `{secret:<key>}`
    Secret(&'a str),
}

/// Split a word into text and placeholders.
fn tokens(text: &str) -> Vec<Token<'_>> {
    let mut out = Vec::new();
    let mut rest = text;
    let mut start = 0;
    while let Some(open) = rest[start..].find('{').map(|i| start + i) {
        let Some(close) = rest[open..].find('}').map(|i| open + i) else {
            break;
        };
        let inner = &rest[open + 1..close];
        let token = match inner.strip_prefix("secret:") {
            Some(key) if is_secret_key(key) => Some(Token::Secret(key)),
            Some(_) => None,
            None if check_name("argument", inner).is_ok() => Some(Token::Arg(inner)),
            None => None,
        };
        match token {
            Some(token) => {
                if open > 0 {
                    out.push(Token::Text(&rest[..open]));
                }
                out.push(token);
                rest = &rest[close + 1..];
                start = 0;
            }
            None => start = open + 1,
        }
    }
    if !rest.is_empty() {
        out.push(Token::Text(rest));
    }
    out
}

fn is_secret_key(key: &str) -> bool {
    !key.is_empty()
        && key
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

fn is_env_name(name: &str) -> bool {
    let mut chars = name.chars();
    chars
        .next()
        .is_some_and(|c| c.is_ascii_uppercase() || c == '_')
        && chars.all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
}

/// What `server cmd` / `app cmd` got after the command's name.
#[derive(Debug, Default, PartialEq)]
pub struct Call {
    /// Every argument, with defaults filled in and values checked.
    pub values: BTreeMap<String, String>,
    pub yes: bool,
    pub dry_run: bool,
    /// `--help`: show the command's usage; values are not checked.
    pub help: bool,
}

/// The command ready to run.
#[derive(Debug, PartialEq)]
pub struct Rendered {
    pub argv: Vec<String>,
    pub env: Vec<(String, String)>,
}

impl Template {
    /// Rules for commands of both files; the caller adds the entry's path.
    /// `secrets`: whether `{secret:…}` can be used in `env`.
    pub fn validate(&self, secrets: bool) -> Result<()> {
        ensure!(
            self.command.first().is_some_and(|word| !word.is_empty()),
            "command: the program and its arguments, as a list"
        );
        let mut optional = None;
        for (name, arg) in &self.args.0 {
            check_name("argument name", name)?;
            match (&arg.default, optional) {
                (Some(_), _) => optional = Some(name),
                (None, Some(before)) => bail!(
                    "args.{name}: comes after `{before}`, which has a default; \
                     put arguments with a default last, so values given in order fill the others first"
                ),
                (None, None) => {}
            }
            ensure!(
                !RESERVED.contains(&name.as_str()),
                "args.{name}: `{name}` is an option of compost; choose another name"
            );
            match arg.kind {
                ArgType::String => {
                    if let Some(pattern) = &arg.pattern {
                        anchored(pattern).map_err(|e| anyhow::anyhow!("args.{name}: {e}"))?;
                    }
                }
                _ if arg.pattern.is_some() => {
                    bail!("args.{name}: `pattern` is only for type string")
                }
                _ => {}
            }
            match arg.kind {
                ArgType::Enum if arg.values.is_empty() => {
                    bail!("args.{name}: type enum needs `values`")
                }
                ArgType::Enum => {}
                _ if !arg.values.is_empty() => {
                    bail!("args.{name}: `values` is only for type enum")
                }
                _ => {}
            }
            if let Some(default) = &arg.default {
                let Some(value) = env_value(default) else {
                    bail!("args.{name}.default: must be a string, number or boolean");
                };
                // An empty default leaves out a word that is only the placeholder.
                if !value.is_empty() {
                    arg.check(&value)
                        .map_err(|e| anyhow::anyhow!("args.{name}.default: {e}"))?;
                }
            }
        }

        for word in &self.command {
            if tokens(word).iter().any(|t| matches!(t, Token::Secret(_))) {
                bail!(
                    "command: `{word}` uses a secret; put it in `env` and use the variable, \
                     so it doesn't show up in the process list"
                );
            }
        }
        for (key, value) in &self.env {
            ensure!(
                is_env_name(key),
                "env.{key}: variable names are uppercase letters, digits and `_`"
            );
            let Some(value) = env_value(value) else {
                bail!("env.{key}: must be a string, number or boolean");
            };
            if !secrets && tokens(&value).iter().any(|t| matches!(t, Token::Secret(_))) {
                bail!(
                    "env.{key}: `{{secret:…}}` is only for commands of compost-server.yaml; \
                     the service's own environment has the app's secrets"
                );
            }
        }
        let words: Vec<String> = self
            .command
            .iter()
            .cloned()
            .chain(self.env.values().filter_map(env_value))
            .collect();
        for (name, _) in &self.args.0 {
            let used = words
                .iter()
                .any(|word| tokens(word).contains(&Token::Arg(name)));
            ensure!(
                used,
                "args.{name}: not used; put `{{{name}}}` in command or env"
            );
        }
        Ok(())
    }

    fn arg(&self, name: &str) -> Option<&Arg> {
        self.args
            .0
            .iter()
            .find(|(arg, _)| arg == name)
            .map(|(_, arg)| arg)
    }

    /// e.g. `unban <ip>`, `kc-sessions [realm]`.
    pub fn usage(&self, name: &str) -> String {
        let mut usage = name.to_string();
        for (arg, spec) in &self.args.0 {
            if spec.default.is_some() {
                let _ = write!(usage, " [{arg}]");
            } else {
                let _ = write!(usage, " <{arg}>");
            }
        }
        usage
    }

    /// `--help` of one command.
    pub fn help(&self, name: &str, place: &str) -> String {
        let mut text = format!("Usage: {}\n", self.usage(name));
        if let Some(description) = &self.description {
            let _ = writeln!(text, "\n{description}");
        }
        let _ = writeln!(text, "\nRuns in {place}: {}", display(&self.command));
        if !self.env.is_empty() {
            let secret = self.secret_env();
            let names: Vec<String> = self
                .env
                .keys()
                .map(|name| match secret.contains(&name.as_str()) {
                    true => format!("{name} (secret)"),
                    false => name.clone(),
                })
                .collect();
            let _ = writeln!(text, "Environment: {}", names.join(", "));
        }
        if !self.args.is_empty() {
            let _ = writeln!(text, "\nArguments (positional, or --<name> <value>):");
            let width = self.args.0.iter().map(|(n, _)| n.len()).max().unwrap_or(0);
            for (arg, spec) in &self.args.0 {
                let mut about = spec.kind.name().to_string();
                let description = spec.description.as_deref().unwrap_or_default();
                if let Some(pattern) = &spec.pattern {
                    let _ = write!(about, " matching `{pattern}`");
                }
                if !spec.values.is_empty() {
                    let _ = write!(about, ": {}", spec.values.join(", "));
                }
                match spec.default_value() {
                    Some(value) if value.is_empty() => about.push_str(", optional"),
                    Some(value) => {
                        let _ = write!(about, ", default {value}");
                    }
                    None => {}
                }
                let about = match description {
                    "" => format!("({about})"),
                    description => format!("{description} ({about})"),
                };
                let _ = writeln!(text, "  {arg:<width$}  {about}");
            }
        }
        let _ = write!(
            text,
            "\nOptions:\n  --dry-run  Show what would run, without running it\n  \
             --yes      Do not ask for confirmation{}",
            if self.confirm {
                ""
            } else {
                " (this command doesn't ask)"
            }
        );
        text
    }

    /// Parse the words after the command's name: values in the order of
    /// `args`, `--<arg> <value>`, `--<arg>=<value>`, `--yes`, `--dry-run`,
    /// `--help`; after `--`, values only.
    pub fn parse_call(&self, name: &str, words: &[String]) -> Result<Call> {
        let usage = || format!("usage: {} (see `{name} --help`)", self.usage(name));
        let mut call = Call::default();
        let mut named: BTreeMap<String, String> = BTreeMap::new();
        let mut positional = Vec::new();
        let mut words = words.iter();
        let mut options = true;
        while let Some(word) = words.next() {
            let Some(option) = word.strip_prefix("--").filter(|_| options) else {
                if options && (word == "-h") {
                    call.help = true;
                } else {
                    positional.push(word.clone());
                }
                continue;
            };
            let (option, inline) = match option.split_once('=') {
                Some((option, value)) => (option, Some(value.to_string())),
                None => (option, None),
            };
            match option {
                "" => options = false,
                "yes" | "dry-run" | "help" if inline.is_some() => {
                    return Err(config_err(format!("--{option} takes no value")));
                }
                "yes" => call.yes = true,
                "dry-run" => call.dry_run = true,
                "help" => call.help = true,
                _ if self.arg(option).is_some() => {
                    let value = match inline {
                        Some(value) => value,
                        None => words.next().cloned().ok_or_else(|| {
                            config_err(format!("--{option} needs a value; {}", usage()))
                        })?,
                    };
                    if named.insert(option.to_string(), value).is_some() {
                        return Err(config_err(format!("--{option} is given more than once")));
                    }
                }
                _ => {
                    return Err(config_err(format!(
                        "{name} has no option --{option}; {}",
                        usage()
                    )));
                }
            }
        }
        if call.help {
            return Ok(call);
        }

        let mut positional = positional.into_iter();
        for (arg, spec) in &self.args.0 {
            let value = match named.remove(arg) {
                Some(value) => value,
                None => match positional.next().or_else(|| spec.default_value()) {
                    Some(value) => value,
                    None => {
                        return Err(config_err(format!("{name} needs <{arg}>; {}", usage())));
                    }
                },
            };
            if !(value.is_empty() && spec.default_value().is_some_and(|d| d.is_empty())) {
                spec.check(&value)
                    .map_err(|e| config_err(format!("{name}: {arg} `{value}` {e:#}")))?;
            }
            call.values.insert(arg.clone(), value);
        }
        if let Some(extra) = positional.next() {
            return Err(config_err(format!(
                "{name}: unexpected `{extra}`; {}",
                usage()
            )));
        }
        Ok(call)
    }

    /// The argv and environment with the values filled in; `secret` returns
    /// the value of a `{secret:<key>}`. A word that is only a placeholder
    /// whose value is empty is left out.
    pub fn render(
        &self,
        values: &BTreeMap<String, String>,
        secret: &mut dyn FnMut(&str) -> Result<String>,
    ) -> Result<Rendered> {
        let mut fill = |text: &str| -> Result<String> {
            let mut out = String::new();
            for token in tokens(text) {
                match token {
                    Token::Text(text) => out.push_str(text),
                    Token::Arg(name) => match values.get(name) {
                        Some(value) => out.push_str(value),
                        None => {
                            let _ = write!(out, "{{{name}}}");
                        }
                    },
                    Token::Secret(key) => out.push_str(&secret(key)?),
                }
            }
            Ok(out)
        };
        let mut argv = Vec::new();
        for word in &self.command {
            let only_arg =
                matches!(tokens(word).as_slice(), [Token::Arg(name)] if values.contains_key(*name));
            let value = fill(word)?;
            if !(only_arg && value.is_empty()) {
                argv.push(value);
            }
        }
        let mut env = Vec::new();
        for (key, value) in &self.env {
            let value = env_value(value).unwrap_or_default();
            env.push((key.clone(), fill(&value)?));
        }
        Ok(Rendered { argv, env })
    }

    /// Variables of `env` whose value uses a `{secret:…}`.
    pub fn secret_env(&self) -> Vec<&str> {
        self.env
            .iter()
            .filter(|(_, value)| {
                env_value(value).is_some_and(|value| {
                    tokens(&value).iter().any(|t| matches!(t, Token::Secret(_)))
                })
            })
            .map(|(key, _)| key.as_str())
            .collect()
    }
}

/// Words as a reader would type them: quoted only when needed.
pub fn display(argv: &[String]) -> String {
    argv.iter()
        .map(|word| {
            let plain = !word.is_empty()
                && word
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || "-_./:=@,+%{}".contains(c));
            if plain {
                word.clone()
            } else {
                shell_quote(word)
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// `--list`-style table of commands: usage, description and where each runs.
pub fn list<'a>(commands: impl Iterator<Item = (&'a String, &'a Template, String)>) -> String {
    let rows: Vec<(String, String, String)> = commands
        .map(|(name, template, place)| {
            let place = if template.confirm {
                format!("{place}, asks")
            } else {
                place
            };
            (
                template.usage(name),
                template.description.clone().unwrap_or_default(),
                place,
            )
        })
        .collect();
    let usage = rows.iter().map(|r| r.0.len()).max().unwrap_or(0);
    let description = rows.iter().map(|r| r.1.len()).max().unwrap_or(0);
    let mut text = String::new();
    for (u, d, p) in rows {
        let line = format!("  {u:<usage$}  {d:<description$}  ({p})");
        let _ = writeln!(text, "{}", line.trim_end());
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    fn template(yaml: &str) -> Template {
        serde_yaml_ng::from_str(yaml).unwrap()
    }

    fn words(text: &str) -> Vec<String> {
        text.split_whitespace().map(str::to_string).collect()
    }

    const UNBAN: &str = "{shared_app: caddy-main, service: crowdsec, args: {ip: {type: ip}}, command: [cscli, decisions, delete, --ip, '{ip}'], confirm: true}";

    #[test]
    fn splits_words_into_placeholders() {
        assert_eq!(
            tokens("--ip={ip}"),
            [Token::Text("--ip="), Token::Arg("ip")]
        );
        assert_eq!(
            tokens("{secret:root_password}"),
            [Token::Secret("root_password")]
        );
        // Not placeholders: kept as text.
        assert_eq!(tokens("{print $1}"), [Token::Text("{print $1}")]);
        assert_eq!(tokens("{{.Name}}"), [Token::Text("{{.Name}}")]);
        assert_eq!(tokens("{} {a}"), [Token::Text("{} "), Token::Arg("a")]);
    }

    #[test]
    fn values_stay_single_words() {
        let unban = template(UNBAN);
        unban.validate(true).unwrap();
        let call = unban
            .parse_call("unban", &words("203.0.113.7 --yes"))
            .unwrap();
        assert!(call.yes && !call.dry_run);
        let rendered = unban.render(&call.values, &mut |_| unreachable!()).unwrap();
        assert_eq!(
            rendered.argv,
            words("cscli decisions delete --ip 203.0.113.7")
        );

        let err = unban
            .parse_call("unban", &["1.2.3.4; rm -rf /data".to_string()])
            .unwrap_err();
        assert!(format!("{err:#}").contains("IPv4 or IPv6"), "{err:#}");
    }

    #[test]
    fn parses_named_positional_and_default_values() {
        let t = template(
            "{shared_app: kc, args: {user: {}, realm: {pattern: '[a-z0-9-]+', default: main}}, command: [x, '{realm}', '{user}']}",
        );
        t.validate(true).unwrap();
        let call = t.parse_call("t", &words("alice")).unwrap();
        assert_eq!(call.values["user"], "alice");
        assert_eq!(call.values["realm"], "main");
        assert!(t.parse_call("t", &[]).is_err());
        let call = t.parse_call("t", &words("--realm=dev bob")).unwrap();
        assert_eq!(
            (&*call.values["realm"], &*call.values["user"]),
            ("dev", "bob")
        );
        // A named value leaves the positional ones to the others.
        let call = t.parse_call("t", &words("--user bob dev")).unwrap();
        assert_eq!(
            (&*call.values["realm"], &*call.values["user"]),
            ("dev", "bob")
        );

        for (input, message) in [
            ("a b c", "unexpected `c`"),
            ("--nope x", "has no option --nope"),
            ("bob Bad", "must match"),
            ("--realm", "needs a value"),
            ("--yes=1", "takes no value"),
        ] {
            let err = t.parse_call("t", &words(input)).unwrap_err();
            assert!(format!("{err:#}").contains(message), "{input}: {err:#}");
        }
        // After `--`, values may look like options; the type still checks them.
        let err = t.parse_call("t", &words("-- -x")).unwrap_err();
        assert!(
            format!("{err:#}").contains("must not start with `-`"),
            "{err:#}"
        );
        assert!(t.parse_call("t", &words("x --help")).unwrap().help);
    }

    #[test]
    fn checks_values_by_type() {
        let check = |kind: &str, value: &str| {
            let arg: Arg = serde_yaml_ng::from_str(kind).unwrap();
            arg.check(value).is_ok()
        };
        assert!(check("{type: int}", "-5"));
        assert!(!check("{type: int}", "5x"));
        assert!(check("{type: ip}", "2001:db8::1"));
        assert!(check("{type: cidr}", "203.0.113.0/24"));
        assert!(!check("{type: cidr}", "203.0.113.0/33"));
        assert!(check("{type: duration}", "4h"));
        assert!(!check("{type: duration}", "soon"));
        assert!(check("{type: email}", "a@example.com"));
        assert!(!check("{type: email}", "a@b"));
        assert!(check("{type: enum, values: [a, b]}", "b"));
        assert!(!check("{type: enum, values: [a, b]}", "c"));
        assert!(!check("{}", "line\nbreak"));
        assert!(!check("{}", "--all"));
        assert!(check("{pattern: '-.*'}", "--all"));
    }

    #[test]
    fn rejects_unsafe_or_wrong_templates() {
        for (yaml, message) in [
            ("{command: []}", "as a list"),
            (
                "{command: [psql, '{secret:root_password}']}",
                "put it in `env`",
            ),
            ("{args: {ip: {}}, command: [x]}", "args.ip: not used"),
            ("{args: {yes: {}}, command: ['{yes}']}", "option of compost"),
            (
                "{args: {a: {type: int, pattern: x}}, command: ['{a}']}",
                "only for type string",
            ),
            (
                "{args: {a: {type: enum}}, command: ['{a}']}",
                "needs `values`",
            ),
            (
                "{args: {a: {type: ip, default: x}}, command: ['{a}']}",
                "args.a.default",
            ),
            ("{env: {lower: x}, command: [x]}", "env.lower"),
            (
                "{args: {a: {pattern: '('}}, command: ['{a}']}",
                "invalid pattern",
            ),
            (
                "{args: {a: {default: x}, b: {}}, command: ['{a}', '{b}']}",
                "put arguments with a default last",
            ),
        ] {
            let err = template(yaml).validate(true).unwrap_err();
            assert!(format!("{err:#}").contains(message), "{yaml}: {err:#}");
        }
        let app = template("{exec: web, env: {P: '{secret:x}'}, command: [x]}");
        assert!(
            format!("{:#}", app.validate(false).unwrap_err())
                .contains("only for commands of compost-server.yaml")
        );
        assert!(app.validate(true).is_ok());
    }

    #[test]
    fn renders_env_with_secrets_and_drops_empty_optional_words() {
        let t = template(
            "{shared_app: pg, args: {db: {default: ''}}, env: {PGPASSWORD: '{secret:root_password}', PGPORT: 5432}, command: [psql, -U, postgres, '{db}']}",
        );
        t.validate(true).unwrap();
        assert_eq!(t.secret_env(), ["PGPASSWORD"]);
        let call = t.parse_call("psql", &[]).unwrap();
        let rendered = t
            .render(&call.values, &mut |key| Ok(format!("<{key}>")))
            .unwrap();
        assert_eq!(rendered.argv, words("psql -U postgres"));
        assert_eq!(
            rendered.env,
            [
                ("PGPASSWORD".to_string(), "<root_password>".to_string()),
                ("PGPORT".to_string(), "5432".to_string())
            ]
        );
        let call = t.parse_call("psql", &words("shop")).unwrap();
        let rendered = t.render(&call.values, &mut |_| Ok(String::new())).unwrap();
        assert_eq!(rendered.argv, words("psql -U postgres shop"));
    }

    #[test]
    fn keeps_the_order_of_args_through_serialization() {
        let t = template("{args: {zeta: {}, alpha: {default: x}}, command: ['{zeta}', '{alpha}']}");
        assert_eq!(t.usage("c"), "c <zeta> [alpha]");
        let again: Template =
            serde_yaml_ng::from_str(&serde_yaml_ng::to_string(&t).unwrap()).unwrap();
        assert_eq!(again.usage("c"), "c <zeta> [alpha]");
    }

    #[test]
    fn shows_words_quoted_only_when_needed() {
        assert_eq!(
            display(&[
                "psql".into(),
                "-c".into(),
                "select 1".into(),
                "--ip=1.2.3.4".into()
            ]),
            "psql -c 'select 1' --ip=1.2.3.4"
        );
    }
}
