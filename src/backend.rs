use crate::ir::TypedProgram;

pub trait Backend {
    fn name(&self) -> &'static str;
    fn emit(&self, program: &TypedProgram) -> String;
}
