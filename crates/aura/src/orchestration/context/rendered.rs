//! Rendered prompt text produced by context types.

/// A rendered block of prompt text produced by a context type.
///
/// Render methods in this module return `RenderedContext` instead of a bare
/// `String`, so prompt-assembly code can tell rendered context apart from
/// arbitrary text. There is no public constructor: only this module's render
/// paths mint values, via the module-scoped [`RenderedContext::new`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenderedContext(String);

impl RenderedContext {
    /// Mint a rendered block. Module-scoped: only render paths inside the
    /// context module produce rendered context.
    pub(super) fn new(text: String) -> Self {
        Self(text)
    }

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
