//! Public CLI spelling and help contracts. Help resolution is deliberately pure:
//! it neither reads a project nor probes/installs a compiler or starts a child.

pub const CONTRACT_VERSION: &str = "ku-cli-contract-v1";

/// This table also supplies the aliases rendered in general/subcommand help.
/// Documentation checks can read the same canonical spelling and scope.
#[derive(Clone, Copy, Debug)]
pub struct AliasContract {
    pub scope: &'static str,
    pub canonical: &'static str,
    pub aliases: &'static str,
    pub note: &'static str,
}

macro_rules! alias_contracts {
    ($(($scope:literal, $canonical:literal, $aliases:literal, $note:literal)),* $(,)?) => {
        pub const ALIASES: &[AliasContract] = &[$(AliasContract {
            scope: $scope, canonical: $canonical, aliases: $aliases, note: $note,
        }),*];

        pub const TOP_LEVEL_HELP: &str = concat!(
            "ku - simple, small, fast language tool\n\n",
            "Usage:\n",
            "  ku <file.ku>          Run exactly one source path (no program arguments)\n",
            "  ku create <name> [--template <template>]\n",
            "  ku create --list      List built-in project templates\n",
            "  ku init [--template <template>]\n",
            "  ku template list      List built-in project templates\n",
            "  ku run [--locked|--offline] [file.ku]\n",
            "  ku check [--json] [--deny-unused] [--locked|--offline] [file.ku]\n",
            "  ku ir <file.ku>       Print checked Ku IR draft\n",
            "  ku llvm <file.ku>     Emit prototype LLVM text IR\n",
            "  ku build [options] [file.ku|project-directory]\n",
            "                        Default: debug profile, runner backend\n",
            "  ku package gc [path]\n",
            "  ku package pack [path]\n",
            "  ku package publish [path]\n",
            "  ku package yank [path]\n",
            "  ku package resolve [path] [--locked|--offline]\n",
            "  ku doctor [--json] [--backend runner|c]\n",
            "  ku doctor --install-runner-toolchain\n",
            "  ku supervise [limits] -- <entry>...\n",
            "  ku service [options] -- <native-program> [args...]\n",
            "  ku version            Print Ku version\n",
            "  ku help [command [subcommand]]\n\n",
            "Run/check/build without a path need a valid ku.mod project entry.\n",
            "Bare `ku` does not run a project; `ku run .` is not a project shorthand.\n",
            "Use `ku help build` or `ku build --help` for command-specific help.\n",
            "Help exits without running code, creating projects or installing tools.\n\n",
            "Aliases and scopes:\n",
            $("  ", $scope, ": ", $canonical, " = ", $aliases, "\n    ", $note, "\n",)*
            "\nBuild output:\n",
            "  Default runner packages the interpreter using the matching Rust toolchain.\n",
            "  --backend c builds a native executable with a supported C compiler.\n",
            "  --emit-c also emits C; it does not switch away from runner.\n",
            "  --backend llvm currently emits text IR, not a linked executable.\n",
            "  Historical `ku build --native FILE` emits C only; adding -o/--output\n",
            "  selects a C executable. --native must immediately follow `build`.\n",
            "  `ku package pack` distributes source/dependencies, not an executable.\n\n",
            "Examples:\n",
            "  ku -v\n  ku create hello\n  ku check\n  ku run\n",
            "  ku ./hello.ku\n",
            "  ku build --backend c --release ./hello.ku -o ./dist/hello\n",
            "  ku doctor --backend runner\n"
        );
    };
}

alias_contracts! {
    ("global", "ku --version", "ku -v, ku -V, ku version", "Top-level -v prints the version; it is not build verbose."),
    ("global", "ku --help", "ku -h, ku -help, ku help", "Use ku help COMMAND or ku COMMAND --help for a command."),
    ("run", "ku run ./hello.ku", "ku ./hello.ku", "Implicit run takes one source path only. Use explicit run for --offline/--locked; no argv forwarding."),
    ("create", "--template TEMPLATE", "-t TEMPLATE", "Example: ku create hello -t http."),
    ("init", "--template TEMPLATE", "-t TEMPLATE", "Example: ku init -t cli."),
    ("template", "ku template list", "ku create --list", "Lists basic, cli, http, json, fs and lib templates."),
    ("build", "--output PATH", "-o PATH", "Pass output paths as separate arguments; quote paths containing spaces."),
    ("build", "--profile release", "--release", "Select the release profile."),
    ("build", "--profile debug", "--debug", "Debug is the default profile."),
    ("build", "--profile small", "--small", "Select the small profile."),
    ("build", "--profile fast", "--fast", "Select the fast profile; this is separate from --release."),
    ("build", "--emit-llvm", "--emit-ll", "Also emit prototype LLVM text IR."),
    ("build", "--verbose", "-v", "Within build, -v prints build logs; it does not print Ku version."),
    ("build", "--backend runner", "--backend interp, --backend interpreter", "Runner is the default and requires the bundle-matching Rust compiler for building."),
    ("build", "--backend c", "--backend native-c", "Native C executable; requires a supported C toolchain."),
    ("build", "--backend llvm", "--backend ll", "Prototype text IR output; not a complete executable link path."),
}

#[derive(Clone, Copy, Debug)]
pub struct CommandContract {
    pub name: &'static str,
    pub usage: &'static str,
    pub description: &'static str,
    pub notes: &'static str,
    /// Spelling followed by an argument, not an equals-sign form.
    pub value_options: &'static [&'static str],
    pub flags: &'static [&'static str],
    pub maximum_paths: usize,
}

const BUILD_VALUES: &[&str] = &["-o", "--output", "--profile", "--target", "--backend"];
const BUILD_FLAGS: &[&str] = &[
    "--release",
    "--debug",
    "--small",
    "--fast",
    "--emit-c",
    "--emit-ir",
    "--emit-llvm",
    "--emit-ll",
    "--clean",
    "--verbose",
    "-v",
    "--lto",
    "--strip",
    "--static",
    "--locked",
    "--offline",
];
const DEPENDENCY_FLAGS: &[&str] = &["--locked", "--offline"];

/// Public commands only; private service-child entry points are not advertised.
pub const COMMANDS: &[CommandContract] = &[
    CommandContract {
        name: "run", usage: "ku run [--locked|--offline] [file.ku]",
        description: "Run a Ku source file or the entry of a valid ku.mod project.",
        notes: "Without a path, use the configured project entry. `ku run .` is not directory shorthand. Implicit `ku ./hello.ku` accepts exactly one source path and no options/argv. Use `ku run ./-hello.ku` for a filename starting with '-'.",
        value_options: &[], flags: DEPENDENCY_FLAGS, maximum_paths: 1,
    },
    CommandContract {
        name: "check", usage: "ku check [--json] [--deny-unused] [--locked|--offline] [file.ku]",
        description: "Check a source file or a valid ku.mod project entry without running it.",
        notes: "--json emits JSON Lines diagnostics. --deny-unused treats unused local bindings as errors. --locked and --offline are mutually exclusive. Help is plain text and is not combined with --json.",
        value_options: &[], flags: &["--json", "--deny-unused", "--locked", "--offline"], maximum_paths: 1,
    },
    CommandContract {
        name: "create", usage: "ku create <name> [--template|-t <template>]\n  ku create --list",
        description: "Create a new project directory from a built-in template.",
        notes: "Default template: basic. Templates: basic, cli, http, json, fs, lib. Existing project contents are not overwritten. --list is equivalent to `ku template list` and takes no project name.",
        value_options: &["--template", "-t"], flags: &["--list"], maximum_paths: 1,
    },
    CommandContract {
        name: "init", usage: "ku init [--template|-t <template>]",
        description: "Initialize the current directory as a Ku project.",
        notes: "Default template: basic. Templates: basic, cli, http, json, fs, lib. Existing ku.mod/src/main.ku are not overwritten.",
        value_options: &["--template", "-t"], flags: &[], maximum_paths: 0,
    },
    CommandContract {
        name: "template", usage: "ku template list",
        description: "Inspect built-in project templates.", notes: "Use `ku template list` or `ku create --list`.",
        value_options: &[], flags: &[], maximum_paths: 0,
    },
    CommandContract {
        name: "template list", usage: "ku template list", description: "List built-in project templates.",
        notes: "Equivalent to `ku create --list`.", value_options: &[], flags: &[], maximum_paths: 0,
    },
    CommandContract {
        name: "ir", usage: "ku ir <file.ku>", description: "Print the checked Ku IR draft.",
        notes: "Accepts one source file; does not execute it.", value_options: &[], flags: &[], maximum_paths: 1,
    },
    CommandContract {
        name: "llvm", usage: "ku llvm <file.ku>", description: "Emit prototype LLVM text IR.",
        notes: "Accepts one source file. This is not a complete executable linker backend.", value_options: &[], flags: &[], maximum_paths: 1,
    },
    CommandContract {
        name: "build", usage: "ku build [options] [file.ku|project-directory]",
        description: "Build a runnable program; defaults to debug profile and runner backend.",
        notes: "Options: --output/-o PATH; --profile debug|release|small|fast; --target host|TARGET; --backend runner|c|llvm; --emit-c; --emit-ir; --emit-llvm/--emit-ll; --clean; --verbose/-v; --lto; --strip; --static; --locked|--offline.\nA pathless build or `ku build .` needs a valid ku.mod entry. --target host is the default host choice. Runner packages the interpreter and requires its bundle-matching Rust compiler. --backend c creates a native executable using a C compiler. --emit-c does not select the C backend. --backend llvm emits text IR only.\nCompatibility: --native must immediately follow build. `ku build --native FILE` emits C only; adding -o/--output builds a C executable. It is not an unconditional --backend c alias.\nExamples:\n  ku build ./hello.ku\n  ku build -v --release ./hello.ku -o ./dist/hello\n  ku build --backend c --release ./hello.ku -o ./dist/hello\n  ku doctor --backend runner",
        value_options: BUILD_VALUES, flags: BUILD_FLAGS, maximum_paths: 1,
    },
    CommandContract {
        name: "package", usage: "ku package <gc|pack|publish|yank|resolve> [path]",
        description: "Manage project source/dependency packages.",
        notes: "Omitted paths use the current project context. `package pack` creates a source/dependency archive, not a runnable executable. Use `ku help package SUBCOMMAND` for details.",
        value_options: &[], flags: &[], maximum_paths: 0,
    },
    CommandContract {
        name: "package gc", usage: "ku package gc [path]", description: "Remove unused package cache entries for a project.",
        notes: "Omitted path uses the current project context.", value_options: &[], flags: &[], maximum_paths: 1,
    },
    CommandContract {
        name: "package pack", usage: "ku package pack [path]", description: "Create a deterministic source/dependency distribution archive.",
        notes: "This is not a runnable program or the maintainer's Ku installation bundle. Omitted path uses the current project context.",
        value_options: &[], flags: &[], maximum_paths: 1,
    },
    CommandContract {
        name: "package publish", usage: "ku package publish [path]", description: "Publish through the configured HTTPS registry.",
        notes: "Requires configured registry authentication. Omitted path uses the current project context. Help does not contact a registry or publish anything.",
        value_options: &[], flags: &[], maximum_paths: 1,
    },
    CommandContract {
        name: "package yank", usage: "ku package yank [path]", description: "Withdraw a published version without deleting its artifact.",
        notes: "Requires configured registry authentication. Omitted path uses the current project context. Help does not contact a registry or change a publication.",
        value_options: &[], flags: &[], maximum_paths: 1,
    },
    CommandContract {
        name: "package resolve", usage: "ku package resolve [path] [--locked|--offline]", description: "Resolve and cache the complete dependency graph.",
        notes: "--locked and --offline are mutually exclusive. Omitted path uses the current project context.",
        value_options: &[], flags: DEPENDENCY_FLAGS, maximum_paths: 1,
    },
    CommandContract {
        name: "doctor", usage: "ku doctor [--json] [--backend runner|c]\n  ku doctor --install-runner-toolchain",
        description: "Diagnose the current Ku installation and available build tools.",
        notes: "Default diagnostics are read-only; optional unrequested backends may warn. --backend runner or --backend c requires that backend to be ready. --json emits a stable JSON object only. --install-runner-toolchain explicitly installs this bundle's required Rust toolchain without changing global defaults or project overrides. KU_RUNNER_RUSTC is an explicit absolute compiler path: it has priority and a mismatched compiler is rejected rather than ignored. Help never probes or installs tools and cannot be combined with --json.",
        value_options: &["--backend"], flags: &["--json", "--install-runner-toolchain"], maximum_paths: 0,
    },
    CommandContract {
        name: "version", usage: "ku version", description: "Print Ku version.",
        notes: "Equivalent top-level calls: ku -v, ku -V, ku --version. `ku build -v` means verbose build output.",
        value_options: &[], flags: &[], maximum_paths: 0,
    },
    CommandContract {
        name: "help", usage: "ku help [command [subcommand]]", description: "Print general or command-specific help without side effects.",
        notes: "General help aliases: ku --help, ku -h, ku -help. Command help: ku help build, ku build --help, ku help package pack, ku package pack --help.",
        value_options: &[], flags: &[], maximum_paths: 0,
    },
    CommandContract {
        name: "supervise", usage: "ku supervise [limits] -- <entry>...", description: "Run isolated workers with OS memory limits.",
        notes: "Uses the existing supervise help and platform contract.", value_options: &[], flags: &[], maximum_paths: 0,
    },
    CommandContract {
        name: "service", usage: "ku service [options] -- <native-program> [args...]", description: "Manage a resident HTTP instance.",
        notes: "Uses the existing service help and platform contract.", value_options: &[], flags: &[], maximum_paths: 0,
    },
];

pub fn command_contract(name: &str) -> Option<&'static CommandContract> {
    COMMANDS.iter().find(|contract| contract.name == name)
}

fn command_help(contract: &CommandContract) -> String {
    // These contracts already document platform limits and lifecycle behavior.
    // Keep one source of truth instead of copying an abbreviated help string.
    match contract.name {
        "supervise" => return crate::process_execution::SUPERVISE_USAGE.to_string(),
        "service" => return crate::service_supervision::SERVICE_USAGE.to_string(),
        _ => {}
    }
    let mut text = format!(
        "Usage:\n  {}\n\n{}\n\n{}\n",
        contract.usage, contract.description, contract.notes
    );
    let scope = contract
        .name
        .split_whitespace()
        .next()
        .unwrap_or(contract.name);
    let mut heading = false;
    for alias in ALIASES.iter().filter(|entry| entry.scope == scope) {
        if !heading {
            text.push_str("\nEquivalent spellings and scope:\n");
            heading = true;
        }
        text.push_str(&format!(
            "  {} = {}\n    {}\n",
            alias.canonical, alias.aliases, alias.note
        ));
    }
    text.push_str("\nHelp: --help, -h, -help, or ku help ");
    text.push_str(contract.name);
    text.push('\n');
    text
}

fn is_help(value: &str) -> bool {
    matches!(value, "--help" | "-h" | "-help")
}

fn topic(args: &[String]) -> Result<(&'static CommandContract, usize), String> {
    let command = args.get(1).map(String::as_str).unwrap_or("");
    if matches!(command, "package" | "template") {
        if let Some(subcommand) = args.get(2).filter(|value| !value.starts_with('-')) {
            let name = format!("{command} {subcommand}");
            return command_contract(&name)
                .map(|contract| (contract, 3))
                .ok_or_else(|| {
                    format!(
                        "unknown {command} command '{subcommand}'\nhelp: use `ku help {command}`"
                    )
                });
        }
    }
    command_contract(command)
        .map(|contract| (contract, 2))
        .ok_or_else(|| format!("unknown command '{command}'\nhelp: use `ku --help`"))
}

/// Return a precise hint for parser errors without changing the existing parser.
pub fn error_hint(args: &[String]) -> String {
    match topic(args) {
        Ok((contract, _)) => format!("help: use `ku help {}`", contract.name),
        Err(_) => "help: use `ku --help`".to_string(),
    }
}

/// Resolve only explicit help requests, before dispatching to any command.
/// `None` leaves all existing non-help parsing (including legacy native forms)
/// intact. A token consumed as an option value is never interpreted as help.
pub fn intercept(args: &[String]) -> Option<Result<String, String>> {
    let command = args.get(1)?.as_str();
    if command == "help" {
        let names: Vec<&str> = args.iter().skip(2).map(String::as_str).collect();
        return Some(if names.is_empty() {
            Ok(TOP_LEVEL_HELP.to_string())
        } else if names.len() == 1 && is_help(names[0]) {
            Ok(command_help(
                command_contract("help").expect("public help contract"),
            ))
        } else if names.len() <= 2 {
            let name = names.join(" ");
            command_contract(&name)
                .map(command_help)
                .ok_or_else(|| format!("unknown help topic '{name}'\nhelp: use `ku --help`"))
        } else {
            Err(
                "too many arguments for 'ku help'\nhelp: use `ku help COMMAND [SUBCOMMAND]`"
                    .to_string(),
            )
        });
    }
    if is_help(command) {
        return Some(if args.len() == 2 {
            Ok(TOP_LEVEL_HELP.to_string())
        } else {
            Err("general help takes no arguments; use `ku help COMMAND [SUBCOMMAND]`".to_string())
        });
    }
    // Child/native argv after -- belong to the existing supervisor/service
    // command, including a literal --help. Do not steal another program's argv.
    if matches!(command, "supervise" | "service") {
        return if args.len() == 3 && is_help(&args[2]) {
            Some(topic(args).map(|(contract, _)| command_help(contract)))
        } else {
            None
        };
    }
    if !args.iter().skip(2).any(|value| is_help(value)) {
        return None;
    }
    let (contract, mut index) = match topic(args) {
        Ok(topic) => topic,
        Err(error) => return Some(Err(error)),
    };
    let native = command == "build" && args.get(2).is_some_and(|value| value == "--native");
    let native_emit_only = native
        && !args
            .iter()
            .skip(3)
            .any(|value| matches!(value.as_str(), "-o" | "--output"));
    if native {
        index += 1;
    }
    let mut found = false;
    let mut json = false;
    let mut paths = 0;
    let mut error = None;
    while let Some(argument) = args.get(index) {
        let value = argument.as_str();
        if is_help(value) {
            found = true;
        } else if !native_emit_only && contract.value_options.contains(&value) {
            if args.get(index + 1).is_some() {
                index += 1;
            } else {
                error.get_or_insert_with(|| format!("missing value after '{value}'"));
            }
        } else if contract.flags.contains(&value)
            && (!native_emit_only || DEPENDENCY_FLAGS.contains(&value))
        {
            json |= value == "--json";
        } else if value.starts_with('-') {
            error.get_or_insert_with(|| format!("unknown {} option '{value}'", contract.name));
        } else {
            paths += 1;
            if paths > contract.maximum_paths {
                error.get_or_insert_with(|| {
                    format!("too many arguments for 'ku {}'", contract.name)
                });
            }
        }
        index += 1;
    }
    if !found {
        return None;
    }
    if json {
        error.get_or_insert_with(|| {
            format!(
                "help cannot be combined with --json; use `ku {} --help`",
                contract.name
            )
        });
    }
    Some(if let Some(error) = error {
        Err(format!("{error}\nhelp: use `ku help {}`", contract.name))
    } else {
        Ok(command_help(contract))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_string()).collect()
    }
    fn help(values: &[&str]) -> Result<String, String> {
        intercept(&argv(values)).expect("explicit help request")
    }

    #[test]
    fn general_aliases_and_both_command_help_forms_match() {
        for alias in ["--help", "-h", "-help", "help"] {
            assert_eq!(help(&["ku", alias]).unwrap(), TOP_LEVEL_HELP);
        }
        for contract in COMMANDS {
            let names: Vec<&str> = contract.name.split_whitespace().collect();
            let mut long = vec!["ku", "help"];
            long.extend(&names);
            let expected = help(&long).unwrap();
            for flag in ["--help", "-h", "-help"] {
                let mut short = vec!["ku"];
                short.extend(&names);
                short.push(flag);
                assert_eq!(help(&short).unwrap(), expected, "{} {flag}", contract.name);
            }
        }
    }

    #[test]
    fn help_does_not_consume_an_option_value_or_another_programs_argv() {
        for args in [
            vec!["ku", "build", "-o", "--help", "hello.ku"],
            vec!["ku", "create", "hello", "-t", "--help"],
            vec!["ku", "service", "--", "server", "--help"],
            vec!["ku", "supervise", "--native", "--", "server", "--help"],
        ] {
            assert!(intercept(&argv(&args)).is_none(), "{args:?}");
        }
        assert!(help(&["ku", "build", "-o", "--help", "hello.ku", "-h"])
            .unwrap()
            .contains("runner backend"));
    }

    #[test]
    fn help_rejects_unknown_topics_flags_and_json_mixing() {
        for args in [
            vec!["ku", "help", "missing"],
            vec!["ku", "package", "missing", "--help"],
            vec!["ku", "build", "--missing", "--help"],
            vec!["ku", "build", "--help", "--missing"],
            vec!["ku", "help", "build", "extra"],
            vec!["ku", "--help", "build"],
            vec!["ku", "check", "--json", "--help"],
            vec!["ku", "doctor", "--help", "--json"],
        ] {
            assert!(help(&args).is_err(), "{args:?}");
        }
    }

    #[test]
    fn implicit_execution_and_legacy_native_parse_are_left_to_existing_dispatch() {
        for args in [
            vec!["ku", "./hello.ku"],
            vec!["ku", "./-hello.ku"],
            vec!["ku", "build", "--native", "hello.ku"],
            vec!["ku", "build", "--native", "hello.ku", "-o", "hello"],
            vec!["ku", "build", "-v", "hello.ku"],
            vec!["ku", "-v"],
            vec!["ku", "run", "--offline", "hello.ku"],
        ] {
            assert!(intercept(&argv(&args)).is_none(), "{args:?}");
        }
        assert!(help(&["ku", "build", "hello.ku", "--native", "--help"]).is_err());
        assert!(help(&["ku", "build", "--native", "hello.ku", "--release", "--help"]).is_err());
        assert!(help(&[
            "ku",
            "build",
            "--native",
            "hello.ku",
            "-o",
            "hello",
            "--release",
            "--help"
        ])
        .is_ok());
    }

    #[test]
    fn current_chinese_and_english_cli_docs_cover_the_alias_contract() {
        let documents = [
            (
                "zh-cn",
                include_str!("../web/src/content/docs/zh-cn/docs/tools/cli.md"),
            ),
            (
                "en",
                include_str!("../web/src/content/docs/en/docs/tools/cli.md"),
            ),
        ];
        for alias in ALIASES {
            for spelling in std::iter::once(alias.canonical).chain(alias.aliases.split(", ")) {
                // Examples use concrete template/output values rather than the
                // contract's uppercase placeholders. Preserve meaningful backend
                // and profile values while checking every actual option token.
                let required = spelling
                    .split_whitespace()
                    .filter(|word| !matches!(*word, "TEMPLATE" | "PATH"))
                    .collect::<Vec<_>>()
                    .join(" ");
                for (language, document) in documents {
                    assert!(
                        document.contains(&required),
                        "{language} is missing {required:?} from {}",
                        alias.scope
                    );
                }
            }
        }
    }

    #[test]
    fn version_and_verbose_scopes_and_every_alias_are_public() {
        for alias in ALIASES {
            assert!(TOP_LEVEL_HELP.contains(alias.canonical));
            assert!(TOP_LEVEL_HELP.contains(alias.aliases));
        }
        let build = help(&["ku", "build", "--help"]).unwrap();
        assert!(build.contains("Within build, -v"));
        assert!(build.contains("--emit-c does not select the C backend"));
        assert!(build.contains("--native must immediately follow build"));
        let doctor = help(&["ku", "doctor", "--help"]).unwrap();
        assert!(doctor.contains("KU_RUNNER_RUSTC"));
        assert!(doctor.contains("--install-runner-toolchain"));
        assert!(!TOP_LEVEL_HELP.contains("ku r "));
    }
}
