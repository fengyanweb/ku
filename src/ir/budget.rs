//! Construction tokens for synchronous lowering, not a byte/RSS or OOM model.
//! Shared by real functions, throwaway inference probes and lifted closures.
use super::{KuError, KuResult, Span};

const MAX_EXPANDED_WORK: usize = 262_144;

#[derive(Clone, Copy)]
pub(super) struct LowerLimits {
    pub max_blocks_per_function: usize,
    pub max_instructions_per_block: usize,
    pub max_work: usize,
}

impl Default for LowerLimits {
    fn default() -> Self {
        Self {
            max_blocks_per_function: 10_000,
            max_instructions_per_block: 10_000,
            max_work: MAX_EXPANDED_WORK,
        }
    }
}

pub(super) struct LowerBudget {
    limits: LowerLimits,
    used: usize,
    failure: Option<KuError>,
}

impl LowerBudget {
    pub fn new(limits: LowerLimits) -> Self {
        Self {
            limits,
            used: 0,
            failure: None,
        }
    }

    pub fn check(&self) -> KuResult<()> {
        match &self.failure {
            Some(error) => Err(error.clone()),
            None => Ok(()),
        }
    }

    fn fail(&mut self, message: &str, span: Span) -> KuError {
        self.failure
            .get_or_insert_with(|| KuError::runtime(format!("IR lowering limit: {message}"), span))
            .clone()
    }

    pub fn spend(&mut self, amount: usize, span: Span) -> KuResult<()> {
        self.check()?;
        let Some(next) = self
            .used
            .checked_add(amount)
            .filter(|next| *next <= self.limits.max_work)
        else {
            return Err(self.fail("expanded work exhausted", span));
        };
        self.used = next;
        Ok(())
    }

    pub fn block(&mut self, admitted: usize, span: Span) -> KuResult<()> {
        self.check()?;
        // The entry is block 0; every reserved ID is counted exactly once.
        // Refuse before the ID increment, including an injected usize::MAX cap.
        if admitted >= self.limits.max_blocks_per_function || admitted == usize::MAX {
            return Err(self.fail("function block count exhausted", span));
        }
        self.spend(1, span)
    }

    pub fn finish_block(&mut self, finalized: usize, span: Span) -> KuResult<()> {
        self.check()?;
        if finalized >= self.limits.max_blocks_per_function {
            return Err(self.fail("function block count exhausted", span));
        }
        Ok(())
    }

    pub fn instruction(&mut self, current: usize, span: Span) -> KuResult<()> {
        self.check()?;
        if current >= self.limits.max_instructions_per_block {
            return Err(self.fail("block instruction count exhausted", span));
        }
        self.spend(1, span)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        ir::{self, IrInst},
        lexer::Lexer,
        parser::Parser,
    };
    use std::{cell::RefCell, rc::Rc};

    fn parsed(source: &str) -> crate::ast::Program {
        Parser::new(Lexer::new(source).lex().unwrap())
            .parse_program()
            .unwrap()
    }

    fn lower(
        source: &str,
        limits: LowerLimits,
    ) -> (KuResult<ir::IrProgram>, Rc<RefCell<LowerBudget>>) {
        let budget = Rc::new(RefCell::new(LowerBudget::new(limits)));
        (
            ir::lower_program_with_budget(&parsed(source), budget.clone()),
            budget,
        )
    }

    #[test]
    fn synchronous_ir_budget_exact_work_one_over_zero_and_overflow_are_sticky() {
        let mut budget = LowerBudget::new(LowerLimits {
            max_work: 2,
            ..LowerLimits::default()
        });
        budget.spend(2, Span::default()).unwrap();
        assert_eq!(budget.used, 2);
        let first = budget.spend(1, Span::default()).unwrap_err();
        assert_eq!(budget.used, 2);
        assert_eq!(
            budget.instruction(usize::MAX, Span::default()).unwrap_err(),
            first
        );
        assert_eq!(budget.spend(0, Span::default()).unwrap_err(), first);
        let mut zero = LowerBudget::new(LowerLimits {
            max_work: 0,
            ..LowerLimits::default()
        });
        zero.spend(0, Span::default()).unwrap();
        assert!(zero.spend(1, Span::default()).is_err());
        let mut huge = LowerBudget::new(LowerLimits {
            max_work: usize::MAX,
            ..LowerLimits::default()
        });
        huge.spend(usize::MAX, Span::default()).unwrap();
        assert!(huge.spend(1, Span::default()).is_err());
        assert_eq!(huge.used, usize::MAX);
    }

    #[test]
    fn synchronous_ir_budget_block_and_instruction_reservations_do_not_spend_on_rejection() {
        let limits = LowerLimits {
            max_blocks_per_function: 2,
            max_instructions_per_block: 2,
            max_work: 20,
        };
        let mut budget = LowerBudget::new(limits);
        budget.block(0, Span::default()).unwrap();
        budget.block(1, Span::default()).unwrap();
        let used = budget.used;
        assert!(budget.block(2, Span::default()).is_err());
        assert_eq!(budget.used, used);
        let mut budget = LowerBudget::new(limits);
        budget.instruction(0, Span::default()).unwrap();
        budget.instruction(1, Span::default()).unwrap();
        let used = budget.used;
        assert!(budget.instruction(2, Span::default()).is_err());
        assert_eq!(budget.used, used);
        let mut budget = LowerBudget::new(LowerLimits {
            max_blocks_per_function: 0,
            ..limits
        });
        assert!(budget.block(0, Span::default()).is_err());
        assert_eq!(budget.used, 0);
    }

    #[test]
    fn synchronous_ir_budget_real_lowering_exact_blocks_and_one_under() {
        let source =
            "fn f(): null! { try {} finally { try {} finally { println(1) } } return ok(null) }";
        let (reference, _) = lower(source, LowerLimits::default());
        let reference = reference.unwrap();
        let count = reference.functions[0].blocks.len();
        assert!(count > 5);
        let (exact, _) = lower(
            source,
            LowerLimits {
                max_blocks_per_function: count,
                ..LowerLimits::default()
            },
        );
        assert_eq!(exact.unwrap(), reference);
        let (over, _) = lower(
            source,
            LowerLimits {
                max_blocks_per_function: count - 1,
                ..LowerLimits::default()
            },
        );
        assert!(over.unwrap_err().message.contains("function block count"));
    }

    #[test]
    fn synchronous_ir_budget_checks_earlier_block_not_only_final_current() {
        let source =
            "fn f(): null! { println(1) println(2) println(3) if (true) {} return ok(null) }";
        let (reference, _) = lower(source, LowerLimits::default());
        let reference = reference.unwrap();
        let blocks = &reference.functions[0].blocks;
        let largest = blocks
            .iter()
            .map(|block| block.instructions.len())
            .max()
            .unwrap();
        assert!(blocks.last().unwrap().instructions.len() < largest);
        let (exact, _) = lower(
            source,
            LowerLimits {
                max_instructions_per_block: largest,
                ..LowerLimits::default()
            },
        );
        assert_eq!(exact.unwrap(), reference);
        let (over, _) = lower(
            source,
            LowerLimits {
                max_instructions_per_block: largest - 1,
                ..LowerLimits::default()
            },
        );
        assert!(over
            .unwrap_err()
            .message
            .contains("block instruction count"));
    }

    #[test]
    fn synchronous_ir_budget_match_origin_backfill_is_admitted() {
        let source = "fn f(): int { println(1) println(2) return match 1 { 1 => 7, _ => 9 } }";
        let (reference, _) = lower(source, LowerLimits::default());
        let reference = reference.unwrap();
        let origin = &reference.functions[0].blocks[0];
        assert!(matches!(
            origin.instructions.last(),
            Some(IrInst::Let { .. })
        ));
        let cap = origin.instructions.len() - 1;
        assert!(reference.functions[0]
            .blocks
            .iter()
            .skip(1)
            .all(|block| block.instructions.len() <= cap));
        let (over, _) = lower(
            source,
            LowerLimits {
                max_instructions_per_block: cap,
                ..LowerLimits::default()
            },
        );
        assert!(over
            .unwrap_err()
            .message
            .contains("block instruction count"));
    }

    #[test]
    fn synchronous_ir_budget_probe_and_closure_share_work_and_exact_limit() {
        let body = "println(1) ".repeat(32);
        let source = format!("fn f() {{ first = fn(): int {{ {body} return 1 }} second = fn(): int {{ {body} return 2 }} return first() + second() }}");
        let (reference, budget) = lower(&source, LowerLimits::default());
        let reference = reference.unwrap();
        assert_eq!(reference.functions.len(), 3);
        let used = budget.borrow().used;
        let retained = reference
            .functions
            .iter()
            .map(|function| {
                function.blocks.len()
                    + function
                        .blocks
                        .iter()
                        .map(|block| block.instructions.len())
                        .sum::<usize>()
            })
            .sum::<usize>();
        assert!(
            used > retained * 2,
            "both probe and real lifted bodies must charge the shared budget"
        );
        let (exact, _) = lower(
            &source,
            LowerLimits {
                max_work: used,
                ..LowerLimits::default()
            },
        );
        assert_eq!(exact.unwrap(), reference);
        let (over, over_budget) = lower(
            &source,
            LowerLimits {
                max_work: used - 1,
                ..LowerLimits::default()
            },
        );
        assert!(over.unwrap_err().message.contains("expanded work"));
        assert_eq!(over_budget.borrow().used, used - 1);
    }

    #[test]
    fn synchronous_ir_budget_is_not_reset_between_functions_or_finally_lifts() {
        let (one, one_budget) = lower("fn first(): int { return 1 }", LowerLimits::default());
        one.unwrap();
        let quota = one_budget.borrow().used;
        let (two, _) = lower(
            "fn first(): int { return 1 } fn second(): int { return 2 }",
            LowerLimits {
                max_work: quota,
                ..LowerLimits::default()
            },
        );
        assert!(two.unwrap_err().message.contains("expanded work"));
        let source = "fn outer(): null! { try {} finally { fn local(): int { println(1) return 2 } local() } return ok(null) }";
        let (reference, budget) = lower(source, LowerLimits::default());
        assert_eq!(
            reference.unwrap().functions.len(),
            4,
            "three real finally copies lift three child bodies"
        );
        let quota = budget.borrow().used;
        let (over, _) = lower(
            source,
            LowerLimits {
                max_work: quota - 1,
                ..LowerLimits::default()
            },
        );
        assert!(over.unwrap_err().message.contains("expanded work"));
    }

    #[test]
    fn synchronous_ir_budget_template_parts_have_an_exact_shared_work_boundary() {
        let source = "fn value(): str { return `before{1}{2}{3}after` }";
        let (reference, budget) = lower(source, LowerLimits::default());
        let reference = reference.unwrap();
        let used = budget.borrow().used;
        let (exact, _) = lower(
            source,
            LowerLimits {
                max_work: used,
                ..LowerLimits::default()
            },
        );
        assert_eq!(exact.unwrap(), reference);
        let (over, over_budget) = lower(
            source,
            LowerLimits {
                max_work: used - 1,
                ..LowerLimits::default()
            },
        );
        let error = over.unwrap_err();
        assert!(error.message.contains("IR lowering limit: expanded work"));
        assert_eq!(over_budget.borrow().used, used - 1);
        assert_eq!(over_budget.borrow().check().unwrap_err(), error);
    }

    #[test]
    fn synchronous_ir_budget_template_refusal_precedes_later_interpolation_parsing() {
        // Deliberately no Checker: the trailing interpolation is malformed.
        // This small helper-level case proves staging is not all performed
        // before admission. It is not a claim that malformed source is legal.
        let (result, budget) = lower(
            "fn value(): str { return `{1}{}` }",
            LowerLimits {
                max_work: 4,
                ..LowerLimits::default()
            },
        );
        let error = result.unwrap_err();
        assert!(error.message.contains("IR lowering limit: expanded work"));
        assert_eq!(budget.borrow().used, 4);
        assert_eq!(budget.borrow().check().unwrap_err(), error);

        // Also let the complete first interpolation lower before refusing the
        // next staging step. Derive the cap from a legal single-part template,
        // not from a guessed number of private lowering operations.
        let (single, single_budget) =
            lower("fn value(): str { return `{1}` }", LowerLimits::default());
        single.unwrap();
        let cap = single_budget.borrow().used;
        assert!(cap > 4);
        let (result, budget) = lower(
            "fn value(): str { return `{1}{}` }",
            LowerLimits {
                max_work: cap,
                ..LowerLimits::default()
            },
        );
        let error = result.unwrap_err();
        assert!(error.message.contains("IR lowering limit: expanded work"));
        assert_eq!(budget.borrow().used, cap);
        assert_eq!(budget.borrow().check().unwrap_err(), error);
    }

    #[test]
    fn synchronous_ir_budget_probe_failure_is_not_swallowed_or_replaced_by_later_error() {
        // No Checker: the second function intentionally has a lowering error.
        // Resource refusal in the first inference probe must win before it.
        let source = "fn first() { println(1) println(2) } fn later() { break }";
        let (result, budget) = lower(
            source,
            LowerLimits {
                max_work: 1,
                ..LowerLimits::default()
            },
        );
        let error = result.unwrap_err();
        assert!(error.message.contains("IR lowering limit: expanded work"));
        assert_eq!(budget.borrow().used, 1);
        assert_eq!(budget.borrow().check().unwrap_err(), error);
    }
}
