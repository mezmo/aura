//! Shared test fixtures for the `init` submodules: a default `InitArgs`,
//! `Prompter` builders over scripted/empty stdin, and fake `ModelLister`s. Only
//! compiled under `cfg(test)`.

use std::path::PathBuf;

use anyhow::Result;

use super::InitArgs;
use super::model_list::{ModelList, ModelLister};
use super::prompt::Prompter;
use super::provider::Provider;
use super::spec::{ConfigSpec, resolve_spec};

pub(crate) fn args() -> InitArgs {
    InitArgs {
        output: PathBuf::from("config.toml"),
        provider: Some(Provider::OpenAI),
        model: Some("gpt-5.1".to_string()),
        api_key_env: None,
        region: None,
        base_url: None,
        name: "assistant".to_string(),
        offline: true,
        non_interactive: true,
        force: false,
    }
}

pub(crate) fn non_interactive() -> Prompter<std::io::Empty> {
    Prompter {
        interactive: false,
        is_tty: false,
        stdin: std::io::empty(),
    }
}

pub(crate) fn scripted(input: &'static str) -> Prompter<&'static [u8]> {
    // is_tty = false so ask_secret reads the scripted stdin (no real tty).
    Prompter {
        interactive: true,
        is_tty: false,
        stdin: input.as_bytes(),
    }
}

pub(crate) struct FailingLister;
impl ModelLister for FailingLister {
    fn list(&self, _: Provider, _: Option<&str>, _: Option<&str>) -> Result<ModelList, String> {
        Err("connection refused".to_string())
    }
}

pub(crate) struct FixedLister(pub(crate) Vec<&'static str>);
impl ModelLister for FixedLister {
    fn list(&self, _: Provider, _: Option<&str>, _: Option<&str>) -> Result<ModelList, String> {
        Ok(ModelList::Verified(
            self.0.iter().map(|s| s.to_string()).collect(),
        ))
    }
}

pub(crate) struct RecordingLister {
    pub(crate) seen_key: std::cell::RefCell<Option<String>>,
    pub(crate) models: Vec<&'static str>,
}
impl ModelLister for RecordingLister {
    fn list(
        &self,
        _: Provider,
        api_key: Option<&str>,
        _: Option<&str>,
    ) -> Result<ModelList, String> {
        *self.seen_key.borrow_mut() = api_key.map(String::from);
        Ok(ModelList::Verified(
            self.models.iter().map(|s| s.to_string()).collect(),
        ))
    }
}

pub(crate) fn no_keys(_: &str) -> bool {
    false
}

pub(crate) fn no_values(_: &str) -> Option<String> {
    None
}

/// The common non-interactive, offline-failing resolution used by most spec
/// and render tests.
pub(crate) fn resolve(a: &InitArgs) -> Result<ConfigSpec> {
    resolve_spec(
        a,
        &mut non_interactive(),
        &FailingLister,
        &no_keys,
        &no_values,
    )
}
