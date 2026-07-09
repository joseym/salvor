//! The `#[derive(Tool)]` procedural macro for the Salvor agent runtime.
//!
//! This crate exists only to host the derive. It is re-exported by
//! `salvor-tools` as `salvor_tools::Tool`, so a tool author depends on one crate
//! and never names this one directly. See the derive's own documentation for
//! what it generates.

use proc_macro::TokenStream;
use proc_macro2::TokenStream as TokenStream2;
use quote::quote;
use syn::spanned::Spanned;
use syn::{Data, DeriveInput, Error, LitStr, parse_macro_input};

/// Derives the [`ToolMeta`] half of the tool contract from struct attributes.
///
/// `ToolMeta` is a tool's static identity: the name a model calls it by, a
/// human-readable description, and its side-effect class. This derive writes
/// that `impl` and nothing else. The behavior half, `ToolHandler` (the typed
/// input, output, and async `call`), is always written by hand; the derive
/// never sees or reasons about it.
///
/// # Usage
///
/// ```ignore
/// use salvor_tools::Tool;
///
/// #[derive(Tool)]
/// #[tool(effect = "write", description = "Create a Jira ticket")]
/// struct CreateTicket;
/// ```
///
/// That expands to an `impl salvor_tools::ToolMeta for CreateTicket` with
/// `NAME = "create_ticket"`, `DESCRIPTION = "Create a Jira ticket"`, and
/// `EFFECT = Effect::Write`.
///
/// # The `#[tool(...)]` attribute
///
/// The derive reads a single `#[tool(...)]` attribute whose keys are:
///
/// - `description = "..."` (**required**): the [`DESCRIPTION`] constant.
/// - `effect = "read" | "idempotent" | "write"` (**required**): the [`EFFECT`]
///   constant. Any other string is a compile error that names the three valid
///   values.
/// - `name = "..."` (optional): overrides [`NAME`]. When omitted, the name is
///   derived from the struct identifier (see below).
///
/// Keys may be split across more than one `#[tool(...)]` attribute or combined
/// in one. Repeating a key, using an unknown key, or giving a key a value that
/// is not a string literal is a compile error.
///
/// # The default name
///
/// With no `name = "..."`, the derive lowercases the struct identifier into
/// `snake_case` by inserting an underscore at each word boundary:
///
/// - before an uppercase letter that follows a lowercase letter or a digit
///   (`CreateTicket` becomes `create_ticket`), and
/// - before an uppercase letter that both follows another uppercase letter and
///   is followed by a lowercase letter, which is the end of an acronym run
///   (`HTTPFetch` becomes `http_fetch`).
///
/// A single-word identifier is simply lowercased (`Ticket` becomes `ticket`).
/// Override the result with `name = "..."` when this rule does not produce the
/// name you want.
///
/// # Hygiene
///
/// The generated code names every path from the crate root: the trait as
/// `::salvor_tools::ToolMeta` and the effect as `::salvor_tools::Effect`. A
/// `use` in the calling module that shadows either name (the tool author's own
/// `Effect` type, say) cannot change what the generated `impl` refers to.
///
/// # What it rejects
///
/// The derive applies only to a `struct`. Placing it on an `enum` or a `union`
/// is a compile error, because a tool's identity is one name, not a choice
/// among several.
///
/// [`ToolMeta`]: ../salvor_tools/trait.ToolMeta.html
/// [`NAME`]: ../salvor_tools/trait.ToolMeta.html#associatedconstant.NAME
/// [`DESCRIPTION`]: ../salvor_tools/trait.ToolMeta.html#associatedconstant.DESCRIPTION
/// [`EFFECT`]: ../salvor_tools/trait.ToolMeta.html#associatedconstant.EFFECT
#[proc_macro_derive(Tool, attributes(tool))]
pub fn derive_tool(input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as DeriveInput);
    expand(input)
        .unwrap_or_else(Error::into_compile_error)
        .into()
}

/// The valid `effect = "..."` values, named in one place so the derive's docs,
/// its error messages, and the mapping below cannot drift apart.
const VALID_EFFECTS: [&str; 3] = ["read", "idempotent", "write"];

/// Builds the `ToolMeta` impl, or returns a spanned error to be turned into a
/// `compile_error!` at the offending tokens.
fn expand(input: DeriveInput) -> Result<TokenStream2, Error> {
    // A tool's identity is a single name, so the derive is meaningful only on a
    // struct. Reject an enum or a union at its defining keyword.
    match &input.data {
        Data::Struct(_) => {}
        Data::Enum(data) => {
            return Err(Error::new(
                data.enum_token.span,
                "`#[derive(Tool)]` applies to a struct, not an enum",
            ));
        }
        Data::Union(data) => {
            return Err(Error::new(
                data.union_token.span,
                "`#[derive(Tool)]` applies to a struct, not a union",
            ));
        }
    }

    let attrs = ToolAttrs::parse(&input)?;

    let ident = &input.ident;
    let name = attrs
        .name
        .map(|lit| lit.value())
        .unwrap_or_else(|| to_snake_case(&ident.to_string()));
    let description = attrs.description.value();
    let effect = attrs.effect;

    // Support a generic tool type, even though tools are typically unit structs.
    let (impl_generics, ty_generics, where_clause) = input.generics.split_for_impl();

    Ok(quote! {
        impl #impl_generics ::salvor_tools::ToolMeta for #ident #ty_generics #where_clause {
            const NAME: &'static str = #name;
            const DESCRIPTION: &'static str = #description;
            const EFFECT: ::salvor_tools::Effect = #effect;
        }
    })
}

/// The three pieces of metadata read from `#[tool(...)]`, after validation.
///
/// `description` and `effect` are non-optional here because a parsed
/// `ToolAttrs` only exists once both were supplied; `name` stays optional
/// because the derive falls back to the struct identifier.
struct ToolAttrs {
    name: Option<LitStr>,
    description: LitStr,
    /// The effect as the token stream of a fully qualified `Effect` variant,
    /// ready to splice into the generated constant.
    effect: TokenStream2,
}

impl ToolAttrs {
    /// Reads every `#[tool(...)]` attribute on the input, validating keys and
    /// values and enforcing that the required keys are present.
    fn parse(input: &DeriveInput) -> Result<Self, Error> {
        let tool_attrs: Vec<_> = input
            .attrs
            .iter()
            .filter(|attr| attr.path().is_ident("tool"))
            .collect();

        // With nothing to point at, anchor the "missing attribute" error on the
        // struct name so the author sees where the attribute belongs.
        let Some(first) = tool_attrs.first() else {
            return Err(Error::new(
                input.ident.span(),
                "`#[derive(Tool)]` needs a `#[tool(...)]` attribute with at least \
                 `description = \"...\"` and `effect = \"...\"`",
            ));
        };
        let anchor = first.span();

        let mut name: Option<LitStr> = None;
        let mut description: Option<LitStr> = None;
        let mut effect: Option<TokenStream2> = None;

        for attr in &tool_attrs {
            attr.parse_nested_meta(|meta| {
                if meta.path.is_ident("name") {
                    set_once(&mut name, &meta, meta.value()?.parse()?)
                } else if meta.path.is_ident("description") {
                    set_once(&mut description, &meta, meta.value()?.parse()?)
                } else if meta.path.is_ident("effect") {
                    let lit: LitStr = meta.value()?.parse()?;
                    set_once(&mut effect, &meta, parse_effect(&lit)?)
                } else {
                    Err(meta.error(
                        "unknown `#[tool(...)]` key; expected one of \
                         `name`, `description`, `effect`",
                    ))
                }
            })?;
        }

        let description = description.ok_or_else(|| {
            Error::new(anchor, "`#[tool(...)]` is missing `description = \"...\"`")
        })?;
        let effect = effect.ok_or_else(|| {
            Error::new(
                anchor,
                "`#[tool(...)]` is missing `effect = \"...\"`; expected one of \
                 \"read\", \"idempotent\", \"write\"",
            )
        })?;

        Ok(Self {
            name,
            description,
            effect,
        })
    }
}

/// Stores a key's value, or errors at the key if it was already set. This is
/// what makes a repeated key (`effect = "read", effect = "write"`) a compile
/// error rather than a silent last-wins.
fn set_once<T>(
    slot: &mut Option<T>,
    meta: &syn::meta::ParseNestedMeta,
    value: T,
) -> Result<(), Error> {
    if slot.is_some() {
        let key = meta
            .path
            .get_ident()
            .map(ToString::to_string)
            .unwrap_or_default();
        return Err(meta.error(format!("duplicate `#[tool(...)]` key `{key}`")));
    }
    *slot = Some(value);
    Ok(())
}

/// Maps an `effect = "..."` string literal to the token stream of the matching
/// fully qualified [`Effect`] variant, or errors at the literal listing the
/// three valid values.
fn parse_effect(lit: &LitStr) -> Result<TokenStream2, Error> {
    match lit.value().as_str() {
        "read" => Ok(quote! { ::salvor_tools::Effect::Read }),
        "idempotent" => Ok(quote! { ::salvor_tools::Effect::Idempotent }),
        "write" => Ok(quote! { ::salvor_tools::Effect::Write }),
        other => Err(Error::new(
            lit.span(),
            format!(
                "invalid effect `{other}`; expected one of {}",
                VALID_EFFECTS
                    .iter()
                    .map(|v| format!("\"{v}\""))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        )),
    }
}

/// Lowercases an identifier into `snake_case`, inserting an underscore at each
/// word boundary. See the derive's documentation for the exact rule this
/// implements.
fn to_snake_case(ident: &str) -> String {
    let chars: Vec<char> = ident.chars().collect();
    let mut out = String::with_capacity(chars.len() + 4);

    for (i, &c) in chars.iter().enumerate() {
        if c.is_uppercase() && i > 0 {
            let prev = chars[i - 1];
            let next = chars.get(i + 1).copied();
            // A boundary is either the start of a word after a lowercase letter
            // or digit, or the tail of an acronym run: an uppercase letter
            // preceded by another uppercase letter but followed by a lowercase
            // one (the `F` in `HTTPFetch`).
            let after_lower = !prev.is_uppercase();
            let acronym_tail = prev.is_uppercase() && next.is_some_and(char::is_lowercase);
            if after_lower || acronym_tail {
                out.push('_');
            }
        }
        out.extend(c.to_lowercase());
    }

    out
}
