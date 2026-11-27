use std::{collections::BTreeSet, ffi::OsString, path::PathBuf};

use subc_core::setup;

use super::CkError;

pub(super) const SETUP_HELP: &str = "ck setup — plan managed CortexKit installation

usage:
  ck setup [<component> | --with <component>,<component>] [--dry-run] [--verbose] [--yes|-y]
  ck setup [<component> | --with <component>,<component>] --key-path <file>
  ck setup <aft|mc> --convert [--confirm]
  ck setup --uninstall [--dry-run]
  ck setup operator-prompt [--uninstall]

  Components: aft, mc, insula, claustrum, synapse, fusiform, cingulate,
  entorhinal, astrocyte, striatum, broca, cerebellum.

  Bare setup installs core and all available components. The plan says why
  a component was left out and marks fusiform, striatum and broca as Windows
  previews. Explicit picks also install their needs: for example,
  ck setup --with cerebellum,cingulate adds claustrum and entorhinal.
  --key-path is valid whenever claustrum is selected, directly or as a need.
  Module settings live in per-module JSONC files. On macOS the Cerebellum
  browser extension is a separate release asset, not a binary.

  --dry-run prints the complete plan without changing anything. Interactive
  setup shows that plan and asks Continue? [y/N] before changes; --yes (-y)
  skips the question. Piped or non-terminal setup proceeds without a question.
  --convert requires --confirm before applying a conversion. --verbose includes
  additional plan outcomes and download diagnostics when a network request fails.";

pub(super) fn parse_setup_command(tail: &[OsString]) -> Result<setup::SetupRequest, CkError> {
    let mut optional = BTreeSet::new();
    let mut explicit_component = None;
    let mut used_with = false;
    let mut uninstall = false;
    let mut dry_run = false;
    let mut verbose = false;
    let mut yes = false;
    let mut convert = false;
    let mut conversion_confirmed = false;
    let mut claustrum_key_path = None;
    let mut index = 0;

    while let Some(argument) = tail.get(index) {
        let argument = argument.to_string_lossy();
        match argument.as_ref() {
            "--with" => {
                let Some(value) = tail.get(index + 1) else {
                    return Err(CkError::Usage(format!(
                        "ck setup --with requires a comma-separated component list\n\n{SETUP_HELP}"
                    )));
                };
                if used_with || explicit_component.is_some() {
                    return Err(CkError::Usage(format!(
                        "ck setup accepts either one explicit component or one --with list\n\n{SETUP_HELP}"
                    )));
                }
                used_with = true;
                let value = value.to_string_lossy();
                if value.is_empty() {
                    return Err(CkError::Usage(format!(
                        "ck setup --with requires at least one component\n\n{SETUP_HELP}"
                    )));
                }
                for component in value.split(',') {
                    let component = parse_setup_component(component)?;
                    if !optional.insert(component) {
                        return Err(CkError::Usage(format!(
                            "ck setup --with repeats component '{component}'\n\n{SETUP_HELP}"
                        )));
                    }
                }
                index += 2;
            }
            "--dry-run" => {
                dry_run = true;
                index += 1;
            }
            "--verbose" => {
                verbose = true;
                index += 1;
            }
            "--yes" | "-y" => {
                yes = true;
                index += 1;
            }
            "--uninstall" => {
                uninstall = true;
                index += 1;
            }
            "--convert" => {
                convert = true;
                index += 1;
            }
            "--confirm" => {
                conversion_confirmed = true;
                index += 1;
            }
            "--key-path" => {
                let Some(value) = tail.get(index + 1) else {
                    return Err(CkError::Usage(format!(
                        "ck setup --key-path requires a file path\n\n{SETUP_HELP}"
                    )));
                };
                if claustrum_key_path.is_some() {
                    return Err(CkError::Usage(format!(
                        "ck setup accepts one --key-path\n\n{SETUP_HELP}"
                    )));
                }
                claustrum_key_path = Some(PathBuf::from(value));
                index += 2;
            }
            value if value.starts_with('-') => {
                return Err(CkError::Usage(format!(
                    "unknown setup flag '{value}'\n\n{SETUP_HELP}"
                )));
            }
            value => {
                if used_with || explicit_component.is_some() {
                    return Err(CkError::Usage(format!(
                        "ck setup accepts one optional component\n\n{SETUP_HELP}"
                    )));
                }
                let component = parse_setup_component(value)?;
                optional.insert(component);
                explicit_component = Some(component);
                index += 1;
            }
        }
    }

    if uninstall && (!optional.is_empty() || convert || conversion_confirmed) {
        return Err(CkError::Usage(format!(
            "ck setup --uninstall cannot be combined with component installation or conversion\n\n{SETUP_HELP}"
        )));
    }
    if convert && explicit_component.is_none() {
        return Err(CkError::Usage(format!(
            "ck setup --convert requires 'aft' or 'mc'\n\n{SETUP_HELP}"
        )));
    }
    if conversion_confirmed && !convert {
        return Err(CkError::Usage(format!(
            "ck setup --confirm is only valid with --convert\n\n{SETUP_HELP}"
        )));
    }
    let mut request = setup::SetupRequest::install(optional.into_iter().collect());
    request.uninstall = uninstall;
    request.dry_run = dry_run;
    request.verbose = verbose;
    request.yes = yes;
    request.convert = if convert { explicit_component } else { None };
    request.conversion_confirmed = conversion_confirmed;
    if request.needs_claustrum() {
        request.claustrum_key_path = match claustrum_key_path {
            Some(path) => Some(path),
            None => setup::default_claustrum_key_path().map_err(CkError::Message)?,
        };
    } else if claustrum_key_path.is_some() {
        return Err(CkError::Usage(format!(
            "ck setup --key-path is only valid when claustrum is selected\n\n{SETUP_HELP}"
        )));
    }
    Ok(request)
}

pub(super) fn parse_setup_component(value: &str) -> Result<setup::Component, CkError> {
    let components = setup::Component::ALL
        .into_iter()
        .filter(|component| *component != setup::Component::Core);
    components
        .clone()
        .find(|component| component.label() == value)
        .ok_or_else(|| {
            let names = components
                .map(setup::Component::label)
                .collect::<Vec<_>>()
                .join(", ");
            CkError::Usage(format!(
                "unknown setup component '{value}'; expected one of: {names}\n\n{SETUP_HELP}"
            ))
        })
}
