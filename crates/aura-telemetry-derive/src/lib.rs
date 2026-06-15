//! `#[derive(Event)]` for `aura-telemetry`.
//!
//! Users do `use aura_telemetry::Event;` and write:
//!
//! ```ignore
//! #[derive(Event)]
//! #[aura_event(name = "server_started")]
//! pub struct ServerStarted {
//!     pub mode: ServerMode,           // enum impls IntoTelemetryProperty
//!     pub agent_count: bool,          // bool ok
//!     // pub raw_prompt: String,      // would NOT compile
//! }
//! ```
//!
//! The macro emits an `impl aura_telemetry::Event for ...` plus a payload
//! builder that calls `into_telemetry_property()` on each field. The
//! `IntoTelemetryProperty` trait is not implemented for `String`,
//! `&str` (only `&'static str`), or any free-form type — that is the
//! compile-time anti-PII gate. If a contributor adds a field whose type
//! does not implement it, the compiler stops them with a message naming
//! the trait, and the audit checklist in `docs/telemetry.md` explains the
//! options (add a typed enum variant, add the doc row, etc.).

use proc_macro::TokenStream;
use quote::quote;
use syn::{parse_macro_input, Data, DeriveInput, Fields, Lit, LitStr};

#[proc_macro_derive(Event, attributes(aura_event))]
pub fn derive_event(input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as DeriveInput);
    let struct_ident = &input.ident;

    // Extract the event name from `#[aura_event(name = "...")]`.
    let event_name = match extract_event_name(&input) {
        Ok(name) => name,
        Err(err) => return err.to_compile_error().into(),
    };

    let fields = match &input.data {
        Data::Struct(s) => match &s.fields {
            Fields::Named(named) => &named.named,
            _ => {
                return syn::Error::new_spanned(
                    struct_ident,
                    "#[derive(Event)] requires a struct with named fields",
                )
                .to_compile_error()
                .into()
            }
        },
        _ => {
            return syn::Error::new_spanned(
                struct_ident,
                "#[derive(Event)] is only valid on structs",
            )
            .to_compile_error()
            .into()
        }
    };

    let inserts = fields.iter().map(|f| {
        let ident = f.ident.as_ref().expect("named field");
        let key = ident.to_string();
        // Calling `into_telemetry_property()` here is the type gate:
        // the field type must implement
        // `aura_telemetry::IntoTelemetryProperty`. `String`, `&str`, and
        // other free-form types deliberately do not.
        quote! {
            properties.insert(
                #key,
                aura_telemetry::IntoTelemetryProperty::into_telemetry_property(self.#ident),
            );
        }
    });

    let expanded = quote! {
        impl aura_telemetry::Event for #struct_ident {
            fn into_payload(self) -> aura_telemetry::EventPayload {
                let mut properties = aura_telemetry::Properties::new();
                #(#inserts)*
                aura_telemetry::EventPayload {
                    name: #event_name,
                    properties,
                }
            }
        }
    };

    expanded.into()
}

fn extract_event_name(input: &DeriveInput) -> syn::Result<LitStr> {
    for attr in &input.attrs {
        if !attr.path().is_ident("aura_event") {
            continue;
        }
        let mut found: Option<LitStr> = None;
        attr.parse_nested_meta(|meta| {
            if meta.path.is_ident("name") {
                let value = meta.value()?;
                let lit: Lit = value.parse()?;
                match lit {
                    Lit::Str(s) => {
                        found = Some(s);
                        Ok(())
                    }
                    other => Err(syn::Error::new_spanned(
                        other,
                        "`name` must be a string literal",
                    )),
                }
            } else {
                Err(meta.error("unknown aura_event key"))
            }
        })?;
        if let Some(name) = found {
            return Ok(name);
        }
    }
    Err(syn::Error::new_spanned(
        &input.ident,
        "#[derive(Event)] requires #[aura_event(name = \"...\")]",
    ))
}
