use super::{Literal, MatchPattern};
use std::{fmt, slice};

impl MatchPattern {
    /// Visit parents before their fields, retaining field order. Slice cursors
    /// keep the pending storage proportional to depth rather than width.
    pub(crate) fn preorder(&self) -> impl Iterator<Item = &Self> {
        Preorder {
            root: Some(self),
            children: Vec::new(),
        }
    }
}

struct Preorder<'a> {
    root: Option<&'a MatchPattern>,
    children: Vec<slice::Iter<'a, MatchPattern>>,
}

impl<'a> Iterator for Preorder<'a> {
    type Item = &'a MatchPattern;

    fn next(&mut self) -> Option<Self::Item> {
        let node = if let Some(root) = self.root.take() {
            root
        } else {
            loop {
                let children = self.children.last_mut()?;
                if let Some(child) = children.next() {
                    break child;
                }
                self.children.pop();
            }
        };
        if let MatchPattern::EnumVariant { fields, .. } = node {
            if !fields.is_empty() {
                self.children.push(fields.iter());
            }
        }
        Some(node)
    }
}

struct CloneFrame<'a> {
    enum_name: &'a str,
    variant: &'a str,
    remaining: slice::Iter<'a, MatchPattern>,
    fields: Vec<MatchPattern>,
}

impl Clone for MatchPattern {
    fn clone(&self) -> Self {
        let mut frames: Vec<CloneFrame<'_>> = Vec::new();
        let mut current = self;
        loop {
            let mut completed = match current {
                Self::Wildcard => Self::Wildcard,
                Self::Binding(name) => Self::Binding(name.clone()),
                Self::Literal(literal) => Self::Literal(literal.clone()),
                Self::EnumVariant {
                    enum_name,
                    variant,
                    fields,
                } => {
                    if let Some((first, rest)) = fields.split_first() {
                        frames.push(CloneFrame {
                            enum_name,
                            variant,
                            remaining: rest.iter(),
                            fields: Vec::with_capacity(fields.len()),
                        });
                        current = first;
                        continue;
                    }
                    Self::EnumVariant {
                        enum_name: enum_name.clone(),
                        variant: variant.clone(),
                        fields: Vec::new(),
                    }
                }
            };
            loop {
                let Some(frame) = frames.last_mut() else {
                    return completed;
                };
                frame.fields.push(completed);
                if let Some(next) = frame.remaining.next() {
                    current = next;
                    break;
                }
                let frame = frames.pop().expect("completed pattern clone frame");
                completed = Self::EnumVariant {
                    enum_name: frame.enum_name.to_owned(),
                    variant: frame.variant.to_owned(),
                    fields: frame.fields,
                };
            }
        }
    }
}

impl Drop for MatchPattern {
    fn drop(&mut self) {
        let Self::EnumVariant { fields, .. } = self else {
            return;
        };
        if fields.is_empty() {
            return;
        }
        // Reuse the root's field storage as the worklist. Every popped node is
        // detached before its ordinary destructor runs; its reentered Drop
        // therefore sees no children. Patterns contain no runtime resources.
        let mut pending = std::mem::take(fields);
        while let Some(mut node) = pending.pop() {
            if let Self::EnumVariant { fields, .. } = &mut node {
                pending.append(fields);
            }
        }
    }
}

impl PartialEq for MatchPattern {
    fn eq(&self, other: &Self) -> bool {
        let mut left = self.preorder();
        let mut right = other.preorder();
        loop {
            let equal = match (left.next(), right.next()) {
                (None, None) => return true,
                (Some(Self::Wildcard), Some(Self::Wildcard)) => true,
                (Some(Self::Binding(left)), Some(Self::Binding(right))) => left == right,
                (Some(Self::Literal(left)), Some(Self::Literal(right))) => left == right,
                (
                    Some(Self::EnumVariant {
                        enum_name: left_name,
                        variant: left_variant,
                        fields: left_fields,
                    }),
                    Some(Self::EnumVariant {
                        enum_name: right_name,
                        variant: right_variant,
                        fields: right_fields,
                    }),
                ) => {
                    left_name == right_name
                        && left_variant == right_variant
                        && left_fields.len() == right_fields.len()
                }
                _ => false,
            };
            if !equal {
                return false;
            }
        }
    }
}

enum DebugWork<'a> {
    Pattern(&'a MatchPattern, usize),
    Fields(&'a [MatchPattern], usize, bool),
    Literal(&'a Literal, usize),
    Text(&'static str),
    TupleEnd(usize),
    EnumEnd(usize),
}

fn indent(formatter: &mut fmt::Formatter<'_>, depth: usize) -> fmt::Result {
    for _ in 0..depth {
        formatter.write_str("    ")?;
    }
    Ok(())
}

fn tuple_start(formatter: &mut fmt::Formatter<'_>, name: &str, depth: usize) -> fmt::Result {
    formatter.write_str(name)?;
    formatter.write_str("(\n")?;
    indent(formatter, depth + 1)
}

fn tuple_end(formatter: &mut fmt::Formatter<'_>, depth: usize) -> fmt::Result {
    formatter.write_str(",\n")?;
    indent(formatter, depth)?;
    formatter.write_str(")")
}

impl fmt::Debug for MatchPattern {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let pretty = formatter.alternate();
        let mut pending = vec![DebugWork::Pattern(self, 0)];
        while let Some(work) = pending.pop() {
            match work {
                DebugWork::Pattern(pattern, depth) => match pattern {
                    Self::Wildcard => formatter.write_str("Wildcard")?,
                    Self::Binding(name) => {
                        if pretty {
                            tuple_start(formatter, "Binding", depth)?;
                            fmt::Debug::fmt(name, formatter)?;
                            tuple_end(formatter, depth)?;
                        } else {
                            formatter.write_str("Binding(")?;
                            fmt::Debug::fmt(name, formatter)?;
                            formatter.write_str(")")?;
                        }
                    }
                    Self::Literal(literal) => {
                        if pretty {
                            tuple_start(formatter, "Literal", depth)?;
                            pending.push(DebugWork::TupleEnd(depth));
                            pending.push(DebugWork::Literal(literal, depth + 1));
                        } else {
                            formatter.write_str("Literal(")?;
                            fmt::Debug::fmt(literal, formatter)?;
                            formatter.write_str(")")?;
                        }
                    }
                    Self::EnumVariant {
                        enum_name,
                        variant,
                        fields,
                    } => {
                        if pretty {
                            formatter.write_str("EnumVariant {\n")?;
                            indent(formatter, depth + 1)?;
                            formatter.write_str("enum_name: ")?;
                            fmt::Debug::fmt(enum_name, formatter)?;
                            formatter.write_str(",\n")?;
                            indent(formatter, depth + 1)?;
                            formatter.write_str("variant: ")?;
                            fmt::Debug::fmt(variant, formatter)?;
                            formatter.write_str(",\n")?;
                            indent(formatter, depth + 1)?;
                            formatter.write_str("fields: ")?;
                            if fields.is_empty() {
                                formatter.write_str("[],\n")?;
                                indent(formatter, depth)?;
                                formatter.write_str("}")?;
                            } else {
                                formatter.write_str("[\n")?;
                                pending.push(DebugWork::EnumEnd(depth));
                                pending.push(DebugWork::Fields(fields, depth + 2, true));
                            }
                        } else {
                            formatter.write_str("EnumVariant { enum_name: ")?;
                            fmt::Debug::fmt(enum_name, formatter)?;
                            formatter.write_str(", variant: ")?;
                            fmt::Debug::fmt(variant, formatter)?;
                            formatter.write_str(", fields: [")?;
                            pending.push(DebugWork::Text("] }"));
                            pending.push(DebugWork::Fields(fields, depth, true));
                        }
                    }
                },
                DebugWork::Fields(fields, depth, first) => {
                    if let Some((field, rest)) = fields.split_first() {
                        if pretty {
                            indent(formatter, depth)?;
                        } else if !first {
                            formatter.write_str(", ")?;
                        }
                        pending.push(DebugWork::Fields(rest, depth, false));
                        if pretty {
                            pending.push(DebugWork::Text(",\n"));
                        }
                        pending.push(DebugWork::Pattern(field, depth));
                    }
                }
                DebugWork::Literal(literal, depth) => {
                    // Literal has no recursive members. Write its tuple at the
                    // current relative indent while forwarding numeric flags.
                    macro_rules! literal_tuple {
                        ($name:literal, $value:expr) => {{
                            tuple_start(formatter, $name, depth)?;
                            fmt::Debug::fmt($value, formatter)?;
                            tuple_end(formatter, depth)?;
                        }};
                    }
                    match literal {
                        Literal::Int(value) => literal_tuple!("Int", value),
                        Literal::Float(value) => literal_tuple!("Float", value),
                        Literal::Bool(value) => literal_tuple!("Bool", value),
                        Literal::String(value) => literal_tuple!("String", value),
                        Literal::TemplateString(value) => literal_tuple!("TemplateString", value),
                        Literal::Null => formatter.write_str("Null")?,
                    }
                }
                DebugWork::Text(text) => formatter.write_str(text)?,
                DebugWork::TupleEnd(depth) => tuple_end(formatter, depth)?,
                DebugWork::EnumEnd(depth) => {
                    indent(formatter, depth + 1)?;
                    formatter.write_str("],\n")?;
                    indent(formatter, depth)?;
                    formatter.write_str("}")?;
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fmt::Write;

    // A shallow copy of the original derive is a byte-level formatting oracle.
    #[derive(Debug, Clone, PartialEq)]
    enum ReferencePattern {
        Wildcard,
        Binding(String),
        Literal(Literal),
        EnumVariant {
            enum_name: String,
            variant: String,
            fields: Vec<ReferencePattern>,
        },
    }

    fn from_reference(reference: &ReferencePattern) -> MatchPattern {
        match reference {
            ReferencePattern::Wildcard => MatchPattern::Wildcard,
            ReferencePattern::Binding(name) => MatchPattern::Binding(name.clone()),
            ReferencePattern::Literal(literal) => MatchPattern::Literal(literal.clone()),
            ReferencePattern::EnumVariant {
                enum_name,
                variant,
                fields,
            } => MatchPattern::EnumVariant {
                enum_name: enum_name.clone(),
                variant: variant.clone(),
                fields: fields.iter().map(from_reference).collect(),
            },
        }
    }

    #[derive(Debug)]
    struct Wrapper<T> {
        pattern: T,
        marker: Option<i32>,
    }

    #[test]
    fn pattern_traits_preserve_shallow_derive_contracts_and_preorder() {
        let mut references = vec![
            ReferencePattern::Wildcard,
            ReferencePattern::Binding("中文🙂\0\"\\\n".into()),
            ReferencePattern::Literal(Literal::Int(i64::MIN)),
            ReferencePattern::Literal(Literal::Int(i64::MAX)),
            ReferencePattern::Literal(Literal::Float(-0.0)),
            ReferencePattern::Literal(Literal::Float(1.23456789)),
            ReferencePattern::Literal(Literal::Float(f64::NAN)),
            ReferencePattern::Literal(Literal::Float(f64::INFINITY)),
            ReferencePattern::Literal(Literal::Bool(true)),
            ReferencePattern::Literal(Literal::String("中文\0\"\\\n".into())),
            ReferencePattern::Literal(Literal::TemplateString("{name}".into())),
            ReferencePattern::Literal(Literal::Null),
            ReferencePattern::EnumVariant {
                enum_name: "Empty".into(),
                variant: "Leaf".into(),
                fields: Vec::new(),
            },
        ];
        references.push(ReferencePattern::EnumVariant {
            enum_name: "Outer".into(),
            variant: "Fields".into(),
            fields: references.clone(),
        });
        let patterns = references.iter().map(from_reference).collect::<Vec<_>>();
        for (pattern, reference) in patterns.iter().zip(&references) {
            assert_eq!(format!("{pattern:?}"), format!("{reference:?}"));
            assert_eq!(format!("{pattern:#?}"), format!("{reference:#?}"));
            assert_eq!(format!("{pattern:.2?}"), format!("{reference:.2?}"));
            assert_eq!(format!("{pattern:#.2?}"), format!("{reference:#.2?}"));
            assert_eq!(format!("{pattern:x?}"), format!("{reference:x?}"));
            assert_eq!(format!("{pattern:#X?}"), format!("{reference:#X?}"));
            let actual = Wrapper {
                pattern,
                marker: Some(7),
            };
            let expected = Wrapper {
                pattern: reference,
                marker: Some(7),
            };
            // Read the fields as well as deriving Debug, avoiding dead fields.
            assert_eq!(actual.marker, expected.marker);
            assert_eq!(
                actual.pattern.preorder().count(),
                pattern.preorder().count()
            );
            assert_eq!(format!("{actual:?}"), format!("{expected:?}"));
            assert_eq!(format!("{actual:#?}"), format!("{expected:#?}"));
            let cloned = pattern.clone();
            assert_eq!(format!("{cloned:#?}"), format!("{reference:#?}"));
            assert_eq!(pattern == &cloned, reference == &reference.clone());
        }
        for (left, left_reference) in patterns.iter().zip(&references) {
            for (right, right_reference) in patterns.iter().zip(&references) {
                assert_eq!(left == right, left_reference == right_reference);
            }
        }

        let ordered = MatchPattern::EnumVariant {
            enum_name: "Root".into(),
            variant: "Pair".into(),
            fields: vec![
                MatchPattern::Binding("first".into()),
                MatchPattern::EnumVariant {
                    enum_name: "Child".into(),
                    variant: "Leaf".into(),
                    fields: vec![MatchPattern::Binding("last".into())],
                },
            ],
        };
        let names = ordered
            .preorder()
            .filter_map(|node| match node {
                MatchPattern::Binding(name) => Some(name.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(names, ["first", "last"]);
        let unequal_shape = MatchPattern::EnumVariant {
            enum_name: "Root".into(),
            variant: "Pair".into(),
            fields: vec![MatchPattern::EnumVariant {
                enum_name: "Child".into(),
                variant: "Leaf".into(),
                fields: vec![
                    MatchPattern::Binding("first".into()),
                    MatchPattern::Binding("last".into()),
                ],
            }],
        };
        assert_ne!(ordered, unequal_shape);
    }

    fn chain(depth: usize) -> MatchPattern {
        let mut pattern = MatchPattern::Wildcard;
        for _ in 0..depth {
            pattern = MatchPattern::EnumVariant {
                enum_name: "E".into(),
                variant: "Wrap".into(),
                fields: vec![pattern],
            };
        }
        pattern
    }

    #[derive(Default)]
    struct DebugCount {
        bytes: usize,
        opens: usize,
        closes: usize,
        leaves: usize,
    }

    impl Write for DebugCount {
        fn write_str(&mut self, text: &str) -> fmt::Result {
            self.bytes += text.len();
            self.opens += usize::from(matches!(
                text,
                "EnumVariant {\n" | "EnumVariant { enum_name: "
            ));
            self.closes += usize::from(matches!(text, "}" | "] }"));
            self.leaves += usize::from(text == "Wildcard");
            Ok(())
        }
    }

    #[test]
    fn pattern_traits_finish_deep_clone_compare_format_and_drop_on_a_small_stack() {
        const CHILD: &str = "KU_MATCH_PATTERN_TRAITS_CHILD";
        const COMPLETED: &str = "deep pattern traits regression completed";
        if std::env::var_os(CHILD).is_none() {
            use crate::registry_server::native_test_harness::{run_bounded, OutputLimits};
            let mut command = std::process::Command::new(std::env::current_exe().unwrap());
            command.args([
                "--exact", "ast::match_pattern::tests::pattern_traits_finish_deep_clone_compare_format_and_drop_on_a_small_stack", "--nocapture",
            ]).env(CHILD, "1");
            let output = run_bounded(
                &mut command,
                std::time::Duration::from_secs(15),
                OutputLimits::new(64 * 1024, 128 * 1024),
            )
            .unwrap();
            assert!(
                output.status.success(),
                "{:?}\n{}\n{}",
                output.status,
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            assert!(String::from_utf8_lossy(&output.stderr)
                .lines()
                .any(|line| line == COMPLETED));
            return;
        }
        std::thread::Builder::new()
            .name("ku-pattern-traits".into())
            .stack_size(128 * 1024)
            .spawn(|| {
                const DEPTH: usize = 8_192;
                let original = chain(DEPTH);
                let cloned = original.clone();
                assert_eq!(original.preorder().count(), DEPTH + 1);
                assert!(original == cloned);
                let mut compact = DebugCount::default();
                write!(&mut compact, "{cloned:?}").unwrap();
                assert_eq!(
                    (compact.opens, compact.closes, compact.leaves),
                    (DEPTH, DEPTH, 1)
                );
                assert_eq!(
                    compact.bytes,
                    DEPTH
                        * ("EnumVariant { enum_name: \"E\", variant: \"Wrap\", fields: [".len()
                            + "] }".len())
                        + "Wildcard".len()
                );
                drop(cloned);
                drop(original);

                // Pretty output necessarily grows with indentation. Count its
                // complete stream rather than retaining a quadratic String.
                const PRETTY_DEPTH: usize = 512;
                let pretty_pattern = chain(PRETTY_DEPTH);
                let mut pretty = DebugCount::default();
                write!(&mut pretty, "{pretty_pattern:#?}").unwrap();
                assert_eq!(
                    (pretty.opens, pretty.closes, pretty.leaves),
                    (PRETTY_DEPTH, PRETTY_DEPTH, 1)
                );
                assert!(pretty.bytes > compact.bytes);
                drop(pretty_pattern);
            })
            .unwrap()
            .join()
            .unwrap();
        eprintln!("{COMPLETED}");
    }
}
