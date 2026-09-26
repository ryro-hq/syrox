use std::collections::HashSet;
use std::path::{Component, Path, PathBuf};

use syrox_lang::{ItemKind, ParsedSources, StringPart};

use super::CheckFailure;

pub(super) fn root_input_locators(
    parsed: &ParsedSources,
) -> Result<Vec<(String, PathBuf)>, CheckFailure> {
    let mut names = HashSet::new();
    let mut locators = Vec::new();
    let main = parsed
        .iter()
        .next()
        .expect("main source was parsed")
        .program();
    for item in &main.items {
        let ItemKind::Inputs(inputs) = &item.kind else {
            continue;
        };
        for input in &inputs.entries {
            if !names.insert(input.name.text.clone()) {
                return Err(CheckFailure::DuplicateInputName {
                    name: input.name.text.clone(),
                });
            }
            let locator = decode_plain_string(&input.value).ok_or_else(|| {
                CheckFailure::UnsupportedLocator {
                    name: input.name.text.clone(),
                    locator: input.value.source.clone(),
                }
            })?;
            let Some(relative) = locator.strip_prefix("path:") else {
                return Err(CheckFailure::UnsupportedLocator {
                    name: input.name.text.clone(),
                    locator,
                });
            };
            let relative = normalize_relative_input(&input.name.text, Path::new(relative))?;
            locators.push((input.name.text.clone(), relative));
        }
    }
    Ok(locators)
}

fn decode_plain_string(value: &syrox_lang::StringLiteral) -> Option<String> {
    let mut decoded = String::new();
    for part in &value.parts {
        let StringPart::Text { source, .. } = part else {
            return None;
        };
        let mut chars = source.chars();
        while let Some(character) = chars.next() {
            if character != '\\' {
                decoded.push(character);
                continue;
            }
            decoded.push(match chars.next()? {
                '"' => '"',
                '\\' => '\\',
                'n' => '\n',
                'r' => '\r',
                't' => '\t',
                '$' => '$',
                _ => return None,
            });
        }
    }
    Some(decoded)
}

fn normalize_relative_input(name: &str, path: &Path) -> Result<PathBuf, CheckFailure> {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Normal(component) => normalized.push(component),
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                return Err(CheckFailure::UnsafeInputPath {
                    name: name.to_owned(),
                    path: path.to_path_buf(),
                });
            }
        }
    }
    Ok(normalized)
}
