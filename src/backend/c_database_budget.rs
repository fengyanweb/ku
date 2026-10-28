//! Process-wide database reservations shared by the generated adapters.

use super::output::COutput;

pub(super) fn emit(out: &mut COutput) {
    out.push_str(include_str!("c_database_budget.c"));
    out.push('\n');
}
