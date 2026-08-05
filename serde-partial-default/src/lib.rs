//! Derive macro for building structs from per-field serde `default`
//! attributes — see [`macro@SerdePartialDefault`].

use proc_macro::TokenStream;
use proc_macro2::TokenStream as TokenStream2;
use quote::quote;
use syn::{Data, DeriveInput, Fields, LitStr, Path};

/// Derives either `impl Default` or a `partial_default(...)` constructor
/// for a struct, depending on whether every field has a default.
///
/// A field is filled in automatically if it carries a serde default
/// attribute — the same one `#[derive(serde::Deserialize)]` already uses:
///
/// - `#[serde(default = "path::to::fn")]` calls `path::to::fn()`.
/// - `#[serde(default)]` calls `<FieldType as Default>::default()`.
///
/// Fields with neither have no default and are taken as parameters.
///
/// - If every field has a default, `SerdePartialDefault` generates a real
///   `impl Default for Self`, so callers write `Self::default()`.
/// - If at least one field has no default, `SerdePartialDefault` generates
///   `pub fn partial_default(<required fields>) -> Self` instead, taking
///   the required fields as parameters in declaration order.
///
/// `SerdePartialDefault` doesn't declare `serde` as its own helper
/// attribute (to avoid ambiguity with serde's own derive), so the struct
/// must also derive `serde::Deserialize` (or `Serialize`) for
/// `#[serde(default...)]` to be valid on its fields at all.
///
/// ```
/// use taceo_serde_partial_default::SerdePartialDefault;
///
/// fn default_retries() -> u32 {
///     3
/// }
///
/// #[derive(serde::Deserialize, SerdePartialDefault)]
/// struct Config {
///     name: String,
///     #[serde(default = "default_retries")]
///     retries: u32,
/// }
///
/// let config = Config {
///     retries: 5,
///     ..Config::partial_default("svc".to_string())
/// };
/// ```
#[proc_macro_derive(SerdePartialDefault)]
pub fn derive_serde_partial_default(input: TokenStream) -> TokenStream {
    let input = syn::parse_macro_input!(input as DeriveInput);
    match expand(&input) {
        Ok(tokens) => tokens.into(),
        Err(err) => err.to_compile_error().into(),
    }
}

enum FieldDefault {
    None,
    Path(Path),
    Default,
}

fn field_default(field: &syn::Field) -> syn::Result<FieldDefault> {
    let mut result = FieldDefault::None;
    for attr in &field.attrs {
        if !attr.path().is_ident("serde") {
            continue;
        }
        attr.parse_nested_meta(|meta| {
            if meta.path.is_ident("default") {
                result = if meta.input.peek(syn::Token![=]) {
                    let lit: LitStr = meta.value()?.parse()?;
                    FieldDefault::Path(lit.parse()?)
                } else {
                    FieldDefault::Default
                };
                Ok(())
            } else if meta.input.peek(syn::Token![=]) {
                let _: TokenStream2 = meta.value()?.parse()?;
                Ok(())
            } else {
                Ok(())
            }
        })?;
    }
    Ok(result)
}

fn expand(input: &DeriveInput) -> syn::Result<TokenStream2> {
    let struct_name = &input.ident;

    let Data::Struct(data) = &input.data else {
        return Err(syn::Error::new_spanned(
            input,
            "Defaults can only be derived for structs",
        ));
    };
    let Fields::Named(named_fields) = &data.fields else {
        return Err(syn::Error::new_spanned(
            input,
            "Defaults can only be derived for structs with named fields",
        ));
    };

    let mut params = Vec::new();
    let mut field_inits = Vec::new();

    for field in &named_fields.named {
        let field_ident = field
            .ident
            .as_ref()
            .ok_or_else(|| syn::Error::new_spanned(field, "Defaults fields must be named"))?;
        let field_ty = &field.ty;

        match field_default(field)? {
            FieldDefault::Path(path) => {
                field_inits.push(quote! { #field_ident: #path() });
            }
            FieldDefault::Default => {
                field_inits.push(quote! { #field_ident: <#field_ty as Default>::default() });
            }
            FieldDefault::None => {
                params.push(quote! { #field_ident: #field_ty });
                field_inits.push(quote! { #field_ident });
            }
        }
    }

    if params.is_empty() {
        Ok(quote! {
            impl Default for #struct_name {
                fn default() -> Self {
                    Self {
                        #(#field_inits),*
                    }
                }
            }
        })
    } else {
        Ok(quote! {
            impl #struct_name {
                /// Builds a `Self` with every serde-defaulted field filled
                /// in; remaining fields are taken as parameters, in
                /// declaration order.
                pub fn partial_default(#(#params),*) -> Self {
                    Self {
                        #(#field_inits),*
                    }
                }
            }
        })
    }
}
