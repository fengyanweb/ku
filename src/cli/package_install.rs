use std::path::PathBuf;

use crate::{
    error::KuResult,
    package::{self, DependencyResolveMode},
    span::Span,
};

use super::{command_error, package_context_from_path, select_dependency_mode};

#[derive(Default)]
struct Options<'a> {
    source: Option<&'a str>,
    mode: Option<DependencyResolveMode>,
    development: bool,
    production: bool,
    python: Option<PathBuf>,
    module: Option<&'a str>,
}

impl<'a> Options<'a> {
    fn parse(args: &'a [String]) -> KuResult<Self> {
        let mut options = Self::default();
        let mut args = args.iter().map(String::as_str);
        while let Some(argument) = args.next() {
            match argument {
                "--dev" if !options.development => options.development = true,
                "--prod" if !options.production => options.production = true,
                "--python" if options.python.is_none() => {
                    options.python = Some(PathBuf::from(required_value(&mut args, argument)?));
                }
                "--module" if options.module.is_none() => {
                    options.module = Some(required_value(&mut args, argument)?);
                }
                "--locked" | "--offline" => {
                    select_dependency_mode(&mut options.mode, argument, "package install")?;
                }
                flag if flag.starts_with('-') => {
                    return Err(command_error(format!(
                        "unknown or duplicate package install option '{flag}'"
                    )));
                }
                source if options.source.is_none() => options.source = Some(source),
                _ => return Err(command_error("package install accepts at most one source")),
            }
        }
        match options.source {
            Some(_) if options.mode.is_some() || options.production => {
                return Err(command_error(
                    "--locked, --offline and --prod restore declared dependencies; remove them when adding a dependency",
                ));
            }
            None if options.development || options.python.is_some() || options.module.is_some() => {
                return Err(command_error(
                    "--dev, --python and --module are used when adding a dependency",
                ));
            }
            _ => {}
        }
        Ok(options)
    }
}

fn required_value<'a>(args: &mut impl Iterator<Item = &'a str>, option: &str) -> KuResult<&'a str> {
    args.next()
        .filter(|value| !value.is_empty() && !value.starts_with('-'))
        .ok_or_else(|| command_error(format!("missing value for '{option}'")))
}

pub(super) fn run(args: &[String]) -> KuResult<()> {
    let options = Options::parse(args)?;
    let mut project = package_context_from_path(std::path::Path::new("."))?;
    let source = crate::interop::prepare_install(
        &project.package_dir,
        options.source,
        options.python.as_deref(),
        options.module,
    )?;
    let mode = options.mode.unwrap_or(DependencyResolveMode::Update);
    let installed = package::install_dependency_with(
        &mut project,
        source.as_deref(),
        options.development,
        options.production,
        mode,
        |resolved, _| {
            for scope in package::package_import_scopes(resolved, Span::default())? {
                if scope.interop_version.as_deref() == Some("1") {
                    super::interop_binding::prepare_with_python(
                        &scope.import_root,
                        &resolved.cache_dir,
                        mode,
                        None,
                        options.python.as_deref(),
                    )?;
                }
            }
            if let Some(python) = options.python.as_deref() {
                super::interop_binding::remember_python(&resolved.cache_dir, python)?;
            }
            Ok(())
        },
    )?;
    if installed.is_empty() {
        println!("package install ok: no dependencies for this scope");
    }
    for receipt in installed {
        println!(
            "package install ok: {}@{} from {} ({})",
            receipt.name,
            receipt.version,
            receipt.source,
            env!("KU_BUILD_TARGET"),
        );
        let mut callable = false;
        for module in receipt.imports {
            if module.exports.is_empty() {
                continue;
            }
            println!(
                "import {{ {} }} from {:?}",
                module.exports.join(", "),
                module.path,
            );
            callable = true;
        }
        if !callable {
            println!("  No callable Ku exports were found in this package.");
        }
    }
    Ok(())
}
