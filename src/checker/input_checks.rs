//! Static checks for the synchronous CLI input builtin.

use super::*;

impl Checker {
    fn check_input_context(&self, span: Span) -> KuResult<()> {
        #[cfg(feature = "browser")]
        if crate::browser::active() {
            return Err(crate::browser::unsupported(
                "input requires a CLI stdin stream",
                span,
            ));
        }
        if cfg!(target_arch = "wasm32") {
            return Err(KuError::structured(
                crate::error::KuErrorKind::Runtime,
                "browser",
                "unsupported_in_browser",
                "unsupported_in_browser: input requires a CLI stdin stream",
                span,
            ));
        }
        if self.async_depth != 0 || self.readonly_capture.is_some() {
            return Err(KuError::runtime(
                "input requires synchronous CLI execution and cannot run inside a Task or HTTP handler",
                span,
            ));
        }
        Ok(())
    }

    pub(super) fn check_input(&mut self, args: &[Expr], span: Span) -> KuResult<Type> {
        self.check_input_context(span)?;
        let ty = match args {
            [] => Type::String,
            [prompt] => match input::template(prompt)? {
                Some(template) if template.fields.len() == 1 => {
                    input_field_type(template.fields[0])
                }
                Some(_) => {
                    return Err(KuError::runtime(
                        "input with multiple placeholders must be destructured with '?'",
                        span,
                    ));
                }
                None => {
                    self.check_argument_modes_conflicts(args, &[ParamMode::View], span)?;
                    let ty = self.check_expr(prompt)?;
                    if !type_matches(&Type::String, &ty) {
                        return Err(type_error(prompt.span, &Type::String, &ty));
                    }
                    Type::String
                }
            },
            _ => {
                return Err(KuError::runtime("input expects zero or one argument", span));
            }
        };
        Ok(Type::Result(Box::new(ty)))
    }

    pub(super) fn check_input_destructure(
        &mut self,
        names: &[Option<String>],
        template: &input::Template,
        span: Span,
    ) -> KuResult<()> {
        self.check_input_context(span)?;
        if names.len() != template.fields.len() {
            return Err(KuError::runtime(
                format!(
                    "input template has {} placeholders for {} destructuring targets",
                    template.fields.len(),
                    names.len()
                ),
                span,
            ));
        }
        self.check_result_propagation(span)?;
        for (name, field) in names.iter().zip(&template.fields) {
            if let Some(name) = name {
                self.assign_or_define_destructured(
                    name,
                    input_field_type(*field),
                    ClosureProvenance::empty(),
                    span,
                )?;
            }
        }
        Ok(())
    }
}

pub(super) fn input_field_type(field: input::Field) -> Type {
    match field {
        input::Field::Text => Type::String,
        input::Field::Int => Type::Int,
    }
}
