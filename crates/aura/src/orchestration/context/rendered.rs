//! Rendered prompt text produced by context types.

/// A rendered block of prompt text produced by a context type.
///
/// Render methods in this module return `RenderedContext` instead of a bare
/// `String`, so prompt-assembly code can tell rendered context apart from
/// arbitrary text. There is no public constructor: only this module's render
/// paths mint values. The implementation cards add a module-scoped
/// constructor together with the render bodies.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenderedContext(String);

impl RenderedContext {
    /// View the rendered text.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl From<RenderedContext> for String {
    fn from(rendered: RenderedContext) -> Self {
        rendered.0
    }
}
