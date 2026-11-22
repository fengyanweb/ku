use super::*;

#[derive(Clone, Debug)]
pub(super) enum BridgeType {
    Void,
    Int(Integer),
    Bool,
    Text,
    Buffer {
        length: usize,
        spelling: &'static str,
    },
}

impl BridgeType {
    pub(super) fn ku_type(&self) -> &'static str {
        match self {
            Self::Void => "null",
            Self::Int(_) => "int",
            Self::Bool => "bool",
            Self::Text | Self::Buffer { .. } => "str",
        }
    }

    pub(super) fn tag(&self) -> &'static str {
        match self {
            Self::Void => "KU_INTEROP_NULL",
            Self::Int(_) => "KU_INTEROP_INT",
            Self::Bool => "KU_INTEROP_BOOL",
            Self::Text | Self::Buffer { .. } => "KU_INTEROP_STR",
        }
    }
}

#[derive(Clone, Debug)]
pub(super) struct Integer {
    pub(super) spelling: String,
    pub(super) minimum: &'static str,
    pub(super) maximum: &'static str,
    pub(super) signed: bool,
}

impl Integer {
    fn parse(name: &str, aliases: &BTreeMap<String, String>) -> Option<Self> {
        let mut underlying = name;
        for _ in 0..32 {
            let known = match underlying {
                "signed char" | "int8_t" => Some(("INT8_MIN", "INT8_MAX", true)),
                "unsigned char" | "uint8_t" => Some(("0", "UINT8_MAX", false)),
                "short" | "short int" | "signed short" | "signed short int" => {
                    Some(("SHRT_MIN", "SHRT_MAX", true))
                }
                "unsigned short" | "unsigned short int" => Some(("0", "USHRT_MAX", false)),
                "int" | "signed" | "signed int" => Some(("INT_MIN", "INT_MAX", true)),
                "unsigned" | "unsigned int" => Some(("0", "UINT_MAX", false)),
                "long" | "long int" | "signed long" | "signed long int" => {
                    Some(("LONG_MIN", "LONG_MAX", true))
                }
                "unsigned long" | "unsigned long int" => Some(("0", "ULONG_MAX", false)),
                "long long" | "long long int" | "signed long long" | "signed long long int" => {
                    Some(("LLONG_MIN", "LLONG_MAX", true))
                }
                "unsigned long long" | "unsigned long long int" => Some(("0", "ULLONG_MAX", false)),
                "int16_t" => Some(("INT16_MIN", "INT16_MAX", true)),
                "uint16_t" => Some(("0", "UINT16_MAX", false)),
                "int32_t" => Some(("INT32_MIN", "INT32_MAX", true)),
                "uint32_t" => Some(("0", "UINT32_MAX", false)),
                "int64_t" => Some(("INT64_MIN", "INT64_MAX", true)),
                "uint64_t" => Some(("0", "UINT64_MAX", false)),
                "size_t" => Some(("0", "SIZE_MAX", false)),
                "ptrdiff_t" => Some(("PTRDIFF_MIN", "PTRDIFF_MAX", true)),
                "intptr_t" => Some(("INTPTR_MIN", "INTPTR_MAX", true)),
                "uintptr_t" => Some(("0", "UINTPTR_MAX", false)),
                _ => None,
            };
            if let Some((minimum, maximum, signed)) = known {
                return Some(Self {
                    spelling: name.to_owned(),
                    minimum,
                    maximum,
                    signed,
                });
            }
            if !identifier(underlying) {
                return None;
            }
            underlying = aliases.get(underlying)?;
        }
        None
    }
}

#[derive(Debug)]
pub(super) enum Operation {
    C {
        symbol: String,
        release: Option<String>,
    },
    Create,
    Destroy,
    Method {
        name: String,
        projection: Option<String>,
    },
}

#[derive(Debug)]
pub(super) struct Export {
    pub(super) name: String,
    pub(super) args: Vec<BridgeType>,
    pub(super) result: BridgeType,
    pub(super) operation: Operation,
}

pub(super) fn load_exports(root: &Path, descriptor: &NativeDescriptor) -> KuResult<Vec<Export>> {
    let header = path_in(root, &descriptor.header, false)?;
    if descriptor.language == Language::Cpp {
        return cpp_exports(descriptor);
    }

    // The Clang driver parses the actual target declarations. No source/library
    // code is executed by this inspection. Use a dedicated Clang frontend
    // when the host compiler is GCC, which does not have -ast-dump.
    let builder = descriptor.language.compiler();
    let clang_builder = Path::new(&builder).file_name().is_some_and(|name| {
        name.to_string_lossy()
            .to_ascii_lowercase()
            .contains("clang")
    });
    let clang = if clang_builder {
        builder.clone()
    } else {
        OsString::from("clang")
    };
    let mut command = Command::new(&clang);
    command
        .current_dir(root)
        .args([
            "-x",
            "c",
            "-std=gnu11",
            "-O2",
            "-fsyntax-only",
            "-fno-color-diagnostics",
            "-Xclang",
            "-ast-dump",
        ])
        .arg("-I")
        .arg(root);
    if cfg!(windows) && !clang_builder {
        configure_mingw_clang(&mut command, &builder)?;
    }
    for path in &descriptor.include {
        command.arg("-I").arg(path_in(root, path, true)?);
    }
    command.arg(&header);
    let (status, output) =
        run_build_process_capture_stdout(&mut command, Duration::from_secs(30), 8 * 1024 * 1024)
            .map_err(|error| {
                err(
                    "native_header",
                    format!(
                        "could not inspect '{}' with {clang:?}: {error}",
                        header.display()
                    ),
                )
            })?;
    if !status.success() {
        return Err(err(
            "native_header",
            format!(
                "Clang rejected '{}'; verify its required headers, target and calling conventions",
                header.display()
            ),
        ));
    }
    let text = std::str::from_utf8(&output)
        .map_err(|_| err("native_header", "Clang declaration output is not UTF-8"))?;
    c_exports(descriptor, &header, text)
}

fn configure_mingw_clang(command: &mut Command, compiler: &OsString) -> KuResult<()> {
    let target = super::compiler_probe(compiler, "-dumpmachine")?;
    let target = target.trim();
    if !target.ends_with("-mingw32") {
        return Err(err(
            "native_target",
            format!("unsupported Windows C header scanner target '{target}'"),
        ));
    }
    command.arg(format!("--target={target}"));

    // LLVM installed in the Visual Studio toolchain may not automatically
    // discover headers beside a separately installed MinGW GCC driver.
    let program = Path::new(compiler);
    let executable = if program.is_absolute() {
        Some(program.to_path_buf())
    } else {
        std::env::var_os("PATH").and_then(|path| {
            std::env::split_paths(&path)
                .map(|dir| dir.join(program))
                .find(|exe| exe.is_file())
        })
    };
    let prefix = executable.as_deref().and_then(|exe| exe.parent()?.parent());
    if let Some(prefix) = prefix {
        for include in [prefix.join(target).join("include"), prefix.join("include")] {
            if include.is_dir() {
                command.arg("-isystem").arg(include);
            }
        }
    }
    Ok(())
}

fn header_origin(location: &str, header: &str, prior: bool) -> bool {
    let location = location.trim_start_matches('<');
    let normalized = cfg!(windows).then(|| location.replace('\\', "/"));
    let location = normalized.as_deref().unwrap_or(location);
    if location.starts_with("line:") || location.starts_with("col:") {
        return prior;
    }
    location
        .strip_prefix(header)
        .is_some_and(|suffix| suffix.starts_with(':'))
}

fn quoted_decl(text: &str) -> Option<(&str, &str)> {
    let text = text.rsplit_once('>').map_or(text, |(_, decl)| decl);
    let (before_type, rest) = text.split_once('\'')?;
    let (ty, _) = rest.split_once('\'')?;
    let name = before_type.split_whitespace().last()?;
    identifier(name).then_some((name, ty))
}

fn c_exports(descriptor: &NativeDescriptor, header: &Path, dump: &str) -> KuResult<Vec<Export>> {
    let header = header.to_string_lossy().replace('\\', "/");
    let mut aliases = BTreeMap::new();
    let mut declared = BTreeMap::new();
    let mut is_in_header = false;
    for line in dump.lines() {
        let Some(top) = line.strip_prefix("|-").or_else(|| line.strip_prefix("`-")) else {
            continue;
        };
        if let Some((_, location)) = top.split_once('<') {
            is_in_header = header_origin(location, &header, is_in_header);
        }
        if top.starts_with("TypedefDecl ") {
            if let Some((name, ty)) = quoted_decl(top) {
                aliases.insert(name.to_owned(), ty.to_owned());
            }
        }
        if top.starts_with("FunctionDecl ") && is_in_header {
            if let Some((name, ty)) = quoted_decl(top) {
                let signature = (ty.to_owned(), top.to_owned());
                if declared
                    .get(name)
                    .is_some_and(|old: &(String, String)| old.0 != signature.0)
                {
                    return Err(err(
                        "native_header",
                        format!("conflicting public declaration of C function '{name}'"),
                    ));
                }
                declared.insert(name.to_owned(), signature);
            }
        }
    }
    let mut exports = Vec::new();
    if descriptor.exports.is_empty() {
        for (symbol, (signature, node)) in &declared {
            let public = exported_name(symbol);
            if let Some(export) =
                scalar_export(&public, symbol, signature, node, &aliases, descriptor)?
            {
                if exports.iter().any(|item: &Export| item.name == public) {
                    return Err(err(
                        "native_header",
                        format!("derived export '{public}' is ambiguous; declare export aliases"),
                    ));
                }
                exports.push(export);
                if exports.len() > MAX_EXPORTS {
                    return Err(err(
                        "native_header",
                        "too many supported declarations; select explicit exports in ku.interop",
                    ));
                }
            }
        }
    } else {
        for (public, symbol) in &descriptor.exports {
            let (signature, node) = declared.get(symbol).ok_or_else(|| {
                err(
                    "native_header",
                    format!(
                        "C function '{symbol}' is not declared by '{}' itself",
                        header
                    ),
                )
            })?;
            let export = scalar_export(public, symbol, signature, node, &aliases, descriptor)?
                .ok_or_else(|| err("native_signature", format!("cannot expose {symbol} {signature}; declare string ownership/borrowing or supply a narrow C scalar shim")))?;
            exports.push(export);
        }
    }
    if exports.is_empty() {
        return Err(err("native_header", "header has no directly supported functions; declare exports or supply a narrow C scalar shim"));
    }
    Ok(exports)
}

fn exported_name(symbol: &str) -> String {
    let mut name = String::new();
    for word in symbol.split('_') {
        if let Some((first, tail)) = word.split_at_checked(1) {
            name.push_str(&first.to_ascii_uppercase());
            name.push_str(tail);
        }
    }
    if !export_identifier(&name) {
        name.insert_str(0, "Call");
    }
    name
}

fn scalar_export(
    public: &str,
    symbol: &str,
    signature: &str,
    node: &str,
    aliases: &BTreeMap<String, String>,
    descriptor: &NativeDescriptor,
) -> KuResult<Option<Export>> {
    // The host/compiler may not agree on arbitrary attributed calling
    // conventions. The supported C subset uses the default ABI.
    if node.contains("__attribute__") || node.ends_with(" static") {
        return Ok(None);
    }
    let Some((ret, args)) = signature.split_once('(') else {
        return Ok(None);
    };
    let ret = ret.trim_end();
    let Some(args) = args.strip_suffix(')') else {
        return Ok(None);
    };
    if args.is_empty() {
        // In C, f() does not declare the arity.
        return Ok(None);
    }
    let mut arguments = Vec::new();
    if args != "void" {
        for (index, ty) in args.split(',').map(str::trim).enumerate() {
            if let Some(&(address, length)) = descriptor.buffers.get(public) {
                if address == index {
                    let spelling = match ty {
                        "const char *" => "const char *",
                        "const unsigned char *" => "const unsigned char *",
                        "const uint8_t *" => "const uint8_t *",
                        "const void *" => "const void *",
                        _ => return Ok(None),
                    };
                    arguments.push(BridgeType::Buffer { length, spelling });
                    continue;
                }
            }
            let Some(value) = parse_c_type(ty, aliases) else {
                return Ok(None);
            };
            if matches!(value, BridgeType::Void)
                || ty == "char *"
                || (matches!(value, BridgeType::Text)
                    && !descriptor.borrows.get(public).is_some_and(|v| v == "cstr"))
            {
                return Ok(None);
            }
            arguments.push(value);
        }
    }
    if let Some(&(address, length)) = descriptor.buffers.get(public) {
        if !matches!(arguments.get(address), Some(BridgeType::Buffer { .. }))
            || !matches!(arguments.get(length), Some(BridgeType::Int(_)))
        {
            return Ok(None);
        }
    }
    let Some(result) = parse_c_type(ret, aliases) else {
        return Ok(None);
    };
    let release = if matches!(result, BridgeType::Text) {
        match descriptor.returns.get(public).map(String::as_str) {
            Some("borrowed_cstr") if ret == "const char *" => None,
            Some(ownership) if ret == "char *" => {
                let Some(symbol) = ownership
                    .strip_prefix("owned_cstr:")
                    .filter(|name| identifier(name))
                else {
                    return Ok(None);
                };
                Some(symbol.to_owned())
            }
            _ => return Ok(None),
        }
    } else {
        None
    };
    if arguments.len() > 32 || public.len() > 128 {
        return Ok(None);
    }
    Ok(Some(Export {
        name: public.to_owned(),
        args: arguments,
        result,
        operation: Operation::C {
            symbol: symbol.to_owned(),
            release,
        },
    }))
}

fn parse_c_type(ty: &str, aliases: &BTreeMap<String, String>) -> Option<BridgeType> {
    match ty {
        "void" => Some(BridgeType::Void),
        "_Bool" | "bool" => Some(BridgeType::Bool),
        "const char *" | "char *" => Some(BridgeType::Text),
        _ => Integer::parse(ty, aliases).map(BridgeType::Int),
    }
}

fn declared_cpp_type(ty: &str) -> Option<BridgeType> {
    match ty {
        "null" => Some(BridgeType::Void),
        "bool" => Some(BridgeType::Bool),
        "cstr" => Some(BridgeType::Text),
        "int" => Integer::parse("int", &BTreeMap::new()).map(BridgeType::Int),
        "i64" => Integer::parse("int64_t", &BTreeMap::new()).map(BridgeType::Int),
        "size" => Integer::parse("size_t", &BTreeMap::new()).map(BridgeType::Int),
        _ => None,
    }
}

fn cpp_exports(descriptor: &NativeDescriptor) -> KuResult<Vec<Export>> {
    let handle = || {
        BridgeType::Int(Integer {
            spelling: "int64_t".to_owned(),
            minimum: "INT64_MIN",
            maximum: "INT64_MAX",
            signed: true,
        })
    };
    let mut exports = vec![
        Export {
            name: descriptor.constructor.clone().unwrap(),
            args: Vec::new(),
            result: handle(),
            operation: Operation::Create,
        },
        Export {
            name: descriptor.destructor.clone().unwrap(),
            args: vec![handle()],
            result: BridgeType::Void,
            operation: Operation::Destroy,
        },
    ];
    for (public, spec) in &descriptor.methods {
        let invalid = || {
            err("native_signature", format!("invalid C++ member declaration '{public} = {spec}'; use Name(cstr,int,size,i64,bool):type or Borrow()->Name():cstr"))
        };
        let (member, result) = spec.rsplit_once(':').ok_or_else(invalid)?;
        let result = declared_cpp_type(result).ok_or_else(invalid)?;
        let (call, projection) = if let Some((call, projection)) = member.split_once("->") {
            let method = projection
                .strip_suffix("()")
                .filter(|name| identifier(name))
                .ok_or_else(invalid)?;
            (call, Some(method.to_owned()))
        } else {
            (member, None)
        };
        let (name, args) = call.split_once('(').ok_or_else(invalid)?;
        if !identifier(name) {
            return Err(invalid());
        }
        let args = args.strip_suffix(')').ok_or_else(invalid)?;
        let mut parsed = vec![handle()];
        if !args.is_empty() {
            for arg in args.split(',') {
                let ty = declared_cpp_type(arg.trim())
                    .filter(|ty| !matches!(ty, BridgeType::Void))
                    .ok_or_else(invalid)?;
                parsed.push(ty);
            }
        }
        if parsed.len() > 32 || (projection.is_some() && matches!(result, BridgeType::Void)) {
            return Err(invalid());
        }
        exports.push(Export {
            name: public.clone(),
            args: parsed,
            result,
            operation: Operation::Method {
                name: name.to_owned(),
                projection,
            },
        });
    }
    Ok(exports)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn c_ast_discovery_uses_only_header_declarations_and_explicit_void_prototype() {
        let descriptor = NativeDescriptor::parse(
            "kind = \"native\"\nlanguage = \"c\"\nheader = \"sdk.h\"\nsource.1 = \"sdk.c\"\n",
        )
        .unwrap();
        let dump = "\
|-FunctionDecl 0x1 </sys/header.h:1:1, col:13> col:2 NeverExport 'int (void)'\n\
|-TypedefDecl 0x2 </pkg/sdk.h:1:1, col:23> col:8 strange_number 'unsigned short'\n\
|-FunctionDecl 0x3 <line:2:1, col:24> col:4 exact_op 'strange_number (strange_number)'\n\
|-FunctionDecl 0x4 <line:3:1, col:14> col:2 NotAPrototype 'int ()'\n\
`-FunctionDecl 0x5 <line:4:1, col:14> col:2 Pointer 'void (int *)'";
        let exports = c_exports(&descriptor, Path::new("/pkg/sdk.h"), dump).unwrap();
        assert_eq!(exports.len(), 1);
        assert_eq!(exports[0].name, "ExactOp");
        assert!(matches!(&exports[0].result, BridgeType::Int(n) if n.maximum == "USHRT_MAX"));
    }
    #[test]
    fn c_string_and_buffer_require_distinct_explicit_lifetime_contracts() {
        let descriptor = NativeDescriptor::parse(
            r#"kind = "native"
language = "c"
header = "sdk.h"
source.1 = "sdk.c"
export.Sum = "sum_bytes"
export.Copy = "copy_name"
buffer.Sum = "0:1"
borrow.Copy = "cstr"
return.Copy = "owned_cstr:release_name"
"#,
        )
        .unwrap();
        let dump = "|-FunctionDecl 0x1 </pkg/sdk.h:1:1, col:11> col:2 sum_bytes 'int (const unsigned char *, size_t)'\n\
`-FunctionDecl 0x2 <line:2:1, col:24> col:4 copy_name 'char *(const char *)'";
        let exports = c_exports(&descriptor, Path::new("/pkg/sdk.h"), dump).unwrap();
        assert!(
            matches!(exports[0].operation, Operation::C { ref release, .. } if release.as_deref() == Some("release_name"))
        );
        assert!(matches!(
            exports[1].args[0],
            BridgeType::Buffer { length: 1, .. }
        ));
    }
}
