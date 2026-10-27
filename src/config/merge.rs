//! Deep merge of YAML documents loaded with `-f` (spec §7).

use std::fs;
use std::path::Path;

use anyhow::{Context, Result};
use serde_yaml_ng::Value;

use crate::error::config_err;

/// Read the files in order and merge each one into the result of the previous ones.
pub fn load_merged(files: &[impl AsRef<Path>]) -> Result<Value> {
    let mut merged = Value::Null;
    merge_files(&mut merged, files)?;
    Ok(merged)
}

/// Read the files in order and merge each one into `merged`.
pub fn merge_files(merged: &mut Value, files: &[impl AsRef<Path>]) -> Result<()> {
    for file in files {
        let path = file.as_ref();
        let text =
            fs::read_to_string(path).with_context(|| format!("cannot read {}", path.display()))?;
        let value: Value = serde_yaml_ng::from_str(&text)
            .map_err(|e| config_err(format!("{}: {e}", path.display())))?;
        merge(merged, value);
    }
    Ok(())
}

/// Remove the top-level `partial` key from the merged files, and fail when it
/// is still set: a file with `partial: true` (or a hint) needs a later file,
/// the overlay of an environment, with `partial: false`.
pub fn take_partial(merged: &mut Value, files: &[impl AsRef<Path>]) -> Result<()> {
    let Some(partial) = merged
        .as_mapping_mut()
        .and_then(|map| map.remove("partial"))
    else {
        return Ok(());
    };
    let hint = match partial {
        Value::Bool(false) | Value::Null => return Ok(()),
        Value::Bool(true) => format!("pass it with -f after {}", first(files)),
        Value::String(hint) => hint,
        _ => return Err(config_err("partial: true, false or a hint (text)")),
    };
    Err(config_err(format!(
        "{} is partial and needs the overlay of an environment: {hint} (the overlay sets `partial: false`)",
        first(files)
    )))
}

fn first(files: &[impl AsRef<Path>]) -> String {
    files.first().map_or_else(
        || "the configuration".to_string(),
        |f| f.as_ref().display().to_string(),
    )
}

/// Merge `overlay` into `base`:
/// 1. maps merge key by key, recursively;
/// 2. lists whose items all have a `name` key merge by `name` (new items are appended);
/// 3. everything else from `overlay` replaces `base` (including an empty list).
pub fn merge(base: &mut Value, overlay: Value) {
    match (base, overlay) {
        (Value::Mapping(base), Value::Mapping(overlay)) => {
            for (key, value) in overlay {
                if let Some(existing) = base.get_mut(&key) {
                    merge(existing, value);
                } else {
                    base.insert(key, value);
                }
            }
        }
        (Value::Sequence(base), Value::Sequence(overlay))
            if !overlay.is_empty() && all_named(base) && all_named(&overlay) =>
        {
            for item in overlay {
                match base.iter_mut().find(|b| b.get("name") == item.get("name")) {
                    Some(existing) => merge(existing, item),
                    None => base.push(item),
                }
            }
        }
        (base, overlay) => *base = overlay,
    }
}

fn all_named(items: &[Value]) -> bool {
    items.iter().all(|item| item.get("name").is_some())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn yaml(text: &str) -> Value {
        serde_yaml_ng::from_str(text).unwrap()
    }

    fn merged(base: &str, overlay: &str) -> Value {
        let mut base = yaml(base);
        merge(&mut base, yaml(overlay));
        base
    }

    #[test]
    fn maps_merge_recursively() {
        assert_eq!(
            merged("a: {x: 1, y: 2}", "a: {y: 3, z: 4}"),
            yaml("a: {x: 1, y: 3, z: 4}")
        );
    }

    #[test]
    fn named_lists_merge_by_name() {
        let base = "db: [{name: pg, version: 16}, {name: other}]";
        let overlay = "db: [{name: pg, configuration: {root_password: x}}, {name: new}]";
        assert_eq!(
            merged(base, overlay),
            yaml(
                "db: [{name: pg, version: 16, configuration: {root_password: x}}, {name: other}, {name: new}]"
            )
        );
    }

    #[test]
    fn plain_lists_and_scalars_replace() {
        assert_eq!(
            merged("{d: [a, b], v: 1}", "{d: [c], v: 2}"),
            yaml("{d: [c], v: 2}")
        );
    }

    #[test]
    fn empty_list_replaces_named_list() {
        assert_eq!(merged("db: [{name: pg}]", "db: []"), yaml("db: []"));
    }

    #[test]
    fn partial_needs_an_overlay_that_completes_it() {
        let files = ["compost-server.yaml"];
        let mut value = yaml("{partial: true, shared: {}}");
        let error = take_partial(&mut value, &files).unwrap_err().to_string();
        assert!(error.contains("compost-server.yaml is partial"), "{error}");
        assert!(error.contains("-f after compost-server.yaml"), "{error}");

        let mut value = yaml("{partial: 'add compost-server.prod.yaml'}");
        let error = take_partial(&mut value, &files).unwrap_err().to_string();
        assert!(error.contains(": add compost-server.prod.yaml"), "{error}");

        let mut value = merged("{partial: true, shared: {}}", "partial: false");
        take_partial(&mut value, &files).unwrap();
        assert_eq!(value, yaml("shared: {}"));

        let mut value = yaml("partial: 1");
        assert!(take_partial(&mut value, &files).is_err());
    }
}
