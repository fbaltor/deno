// Copyright 2018-2022 the Deno authors. All rights reserved. MIT license.

use attrs::Attributes;
use once_cell::sync::Lazy;
use optimizer::{BailoutReason, Optimizer};
use proc_macro::TokenStream;
use proc_macro2::{Span, TokenStream as TokenStream2};
use quote::{quote, ToTokens};
use regex::Regex;
use syn::{
  parse, parse_macro_input, punctuated::Punctuated, token::Comma, FnArg,
  GenericParam, Ident, ItemFn, Lifetime, LifetimeDef,
};

mod attrs;
mod deno;
mod fast_call;
mod optimizer;

const SCOPE_LIFETIME: &str = "'scope";

/// Add the 'scope lifetime to the function signature.
fn add_scope_lifetime(func: &mut ItemFn) {
  let span = Span::call_site();
  let lifetime = LifetimeDef::new(Lifetime::new(SCOPE_LIFETIME, span));
  let generics = &mut func.sig.generics;
  if !generics.lifetimes().any(|def| *def == lifetime) {
    generics.params.push(GenericParam::Lifetime(lifetime));
  }
}

struct Op {
  orig: ItemFn,
  item: ItemFn,
  /// Is this an async op?
  ///   - `async fn`
  ///   - returns a Future
  is_async: bool,
  type_params: Punctuated<GenericParam, Comma>,
  // optimizer: Optimizer,
  core: TokenStream2,
  attrs: Attributes,
}

impl Op {
  fn new(mut item: ItemFn, attrs: Attributes) -> Self {
    // Preserve the original function. Change the name to `call`.
    //
    // impl op_foo {
    //   fn call() {}
    //   ...
    // }
    let mut orig = item.clone();
    orig.sig.ident = Ident::new("call", Span::call_site());

    add_scope_lifetime(&mut item);

    let is_async = item.sig.asyncness.is_some() || is_future(&item.sig.output);
    let type_params = exclude_lifetime_params(&item.sig.generics.params);

    #[cfg(test)]
    let core = quote!(deno_core);
    #[cfg(not(test))]
    let core = deno::import();

    Self {
      orig,
      item,
      type_params,
      is_async,
      core,
      attrs,
    }
  }

  fn gen(mut self) -> TokenStream2 {
    let mut optimizer = Optimizer::new();
    match optimizer.analyze(&mut self) {
      Err(BailoutReason::MustBeSingleSegment)
      | Err(BailoutReason::FastUnsupportedParamType) => {
        optimizer.fast_compatible = false;
      }
      _ => {}
    };

    let Self {
      core,
      item,
      is_async,
      orig,
      attrs,
      type_params,
    } = self;
    let name = &item.sig.ident;
    let generics = &item.sig.generics;
    let where_clause = &item.sig.generics.where_clause;

    // First generate fast call bindings to opt-in to error handling in slow call
    let fast_call::FastImplItems {
      impl_and_fn,
      decl,
      active,
    } = fast_call::generate(&core, &mut optimizer, &item);

    let has_fallible_fast_call = active && optimizer.returns_result;

    let (v8_body, argc) = if is_async {
      codegen_v8_async(
        &core,
        &item,
        attrs,
        item.sig.asyncness.is_some(),
        attrs.deferred,
      )
    } else {
      codegen_v8_sync(&core, &item, attrs, has_fallible_fast_call)
    };

    let is_v8 = attrs.is_v8;
    let is_unstable = attrs.is_unstable;

    let docline = format!("Use `{name}::decl()` to get an op-declaration");
    // Generate wrapper
    quote! {
      #[allow(non_camel_case_types)]
      #[doc="Auto-generated by `deno_ops`, i.e: `#[op]`"]
      #[doc=""]
      #[doc=#docline]
      #[doc="you can include in a `deno_core::Extension`."]
      pub struct #name;

      #[doc(hidden)]
      impl #name {
        pub fn name() -> &'static str {
          stringify!(#name)
        }

        pub fn v8_fn_ptr #generics () -> #core::v8::FunctionCallback #where_clause {
          use #core::v8::MapFnTo;
          Self::v8_func::<#type_params>.map_fn_to()
        }

        pub fn decl #generics () -> #core::OpDecl #where_clause {
          #core::OpDecl {
            name: Self::name(),
            v8_fn_ptr: Self::v8_fn_ptr::<#type_params>(),
            enabled: true,
            fast_fn: #decl,
            is_async: #is_async,
            is_unstable: #is_unstable,
            is_v8: #is_v8,
            argc: #argc,
          }
        }

        #[inline]
        #[allow(clippy::too_many_arguments)]
        #orig

        pub fn v8_func #generics (
          scope: &mut #core::v8::HandleScope<'scope>,
          args: #core::v8::FunctionCallbackArguments,
          mut rv: #core::v8::ReturnValue,
        ) #where_clause {
          #v8_body
        }
      }

      #impl_and_fn
    }
  }
}

#[proc_macro_attribute]
pub fn op(attr: TokenStream, item: TokenStream) -> TokenStream {
  let margs = parse_macro_input!(attr as Attributes);
  let func = parse::<ItemFn>(item).expect("expected a function");
  let op = Op::new(func, margs);
  op.gen().into()
}

/// Generate the body of a v8 func for an async op
fn codegen_v8_async(
  core: &TokenStream2,
  f: &syn::ItemFn,
  margs: Attributes,
  asyncness: bool,
  deferred: bool,
) -> (TokenStream2, usize) {
  let Attributes { is_v8, .. } = margs;
  let special_args = f
    .sig
    .inputs
    .iter()
    .map_while(|a| {
      (if is_v8 { scope_arg(a) } else { None })
        .or_else(|| rc_refcell_opstate_arg(a))
    })
    .collect::<Vec<_>>();
  let rust_i0 = special_args.len();
  let args_head = special_args.into_iter().collect::<TokenStream2>();

  let (arg_decls, args_tail, argc) = codegen_args(core, f, rust_i0, 1);
  let type_params = exclude_lifetime_params(&f.sig.generics.params);

  let (pre_result, mut result_fut) = match asyncness {
    true => (
      quote! {},
      quote! { Self::call::<#type_params>(#args_head #args_tail).await; },
    ),
    false => (
      quote! { let result_fut = Self::call::<#type_params>(#args_head #args_tail); },
      quote! { result_fut.await; },
    ),
  };
  let result_wrapper = match is_result(&f.sig.output) {
    true => {
      // Support `Result<impl Future<Output = Result<T, AnyError>> + 'static, AnyError>`
      if !asyncness {
        result_fut = quote! { result_fut; };
        quote! {
          let result = match result {
            Ok(fut) => fut.await,
            Err(e) => return (promise_id, op_id, #core::_ops::to_op_result::<()>(get_class, Err(e))),
          };
        }
      } else {
        quote! {}
      }
    }
    false => quote! { let result = Ok(result); },
  };

  (
    quote! {
      use #core::futures::FutureExt;
      // SAFETY: #core guarantees args.data() is a v8 External pointing to an OpCtx for the isolates lifetime
      let ctx = unsafe {
        &*(#core::v8::Local::<#core::v8::External>::cast(args.data()).value()
        as *const #core::_ops::OpCtx)
      };
      let op_id = ctx.id;

      let promise_id = args.get(0);
      let promise_id = #core::v8::Local::<#core::v8::Integer>::try_from(promise_id)
        .map(|l| l.value() as #core::PromiseId)
        .map_err(#core::anyhow::Error::from);
      // Fail if promise id invalid (not an int)
      let promise_id: #core::PromiseId = match promise_id {
        Ok(promise_id) => promise_id,
        Err(err) => {
          #core::_ops::throw_type_error(scope, format!("invalid promise id: {}", err));
          return;
        }
      };

      #arg_decls

      // Track async call & get copy of get_error_class_fn
      let get_class = {
        let state = ::std::cell::RefCell::borrow(&ctx.state);
        state.tracker.track_async(op_id);
        state.get_error_class_fn
      };

      #pre_result
      #core::_ops::queue_async_op(ctx, scope, #deferred, async move {
        let result = #result_fut
        #result_wrapper
        (promise_id, op_id, #core::_ops::to_op_result(get_class, result))
      });
    },
    argc,
  )
}

fn scope_arg(arg: &FnArg) -> Option<TokenStream2> {
  if is_handle_scope(arg) {
    Some(quote! { scope, })
  } else {
    None
  }
}

fn opstate_arg(arg: &FnArg) -> Option<TokenStream2> {
  match arg {
    arg if is_rc_refcell_opstate(arg) => Some(quote! { ctx.state.clone(), }),
    arg if is_mut_ref_opstate(arg) => {
      Some(quote! { &mut std::cell::RefCell::borrow_mut(&ctx.state), })
    }
    _ => None,
  }
}

fn rc_refcell_opstate_arg(arg: &FnArg) -> Option<TokenStream2> {
  match arg {
    arg if is_rc_refcell_opstate(arg) => Some(quote! { ctx.state.clone(), }),
    arg if is_mut_ref_opstate(arg) => Some(
      quote! { compile_error!("mutable opstate is not supported in async ops"), },
    ),
    _ => None,
  }
}

/// Generate the body of a v8 func for a sync op
fn codegen_v8_sync(
  core: &TokenStream2,
  f: &syn::ItemFn,
  margs: Attributes,
  has_fallible_fast_call: bool,
) -> (TokenStream2, usize) {
  let Attributes { is_v8, .. } = margs;
  let special_args = f
    .sig
    .inputs
    .iter()
    .map_while(|a| {
      (if is_v8 { scope_arg(a) } else { None }).or_else(|| opstate_arg(a))
    })
    .collect::<Vec<_>>();
  let rust_i0 = special_args.len();
  let args_head = special_args.into_iter().collect::<TokenStream2>();
  let (arg_decls, args_tail, argc) = codegen_args(core, f, rust_i0, 0);
  let ret = codegen_sync_ret(core, &f.sig.output);
  let type_params = exclude_lifetime_params(&f.sig.generics.params);

  let fast_error_handler = if has_fallible_fast_call {
    quote! {
      {
        let op_state = &mut std::cell::RefCell::borrow_mut(&ctx.state);
        if let Some(err) = op_state.last_fast_op_error.take() {
          let exception = #core::error::to_v8_error(scope, op_state.get_error_class_fn, &err);
          scope.throw_exception(exception);
          return;
        }
      }
    }
  } else {
    quote! {}
  };

  (
    quote! {
      // SAFETY: #core guarantees args.data() is a v8 External pointing to an OpCtx for the isolates lifetime
      let ctx = unsafe {
        &*(#core::v8::Local::<#core::v8::External>::cast(args.data()).value()
        as *const #core::_ops::OpCtx)
      };

      #fast_error_handler
      #arg_decls

      let result = Self::call::<#type_params>(#args_head #args_tail);

      // use RefCell::borrow instead of state.borrow to avoid clash with std::borrow::Borrow
      let op_state = ::std::cell::RefCell::borrow(&*ctx.state);
      op_state.tracker.track_sync(ctx.id);

      #ret
    },
    argc,
  )
}

/// (full declarations, idents, v8 argument count)
type ArgumentDecl = (TokenStream2, TokenStream2, usize);

fn codegen_args(
  core: &TokenStream2,
  f: &syn::ItemFn,
  rust_i0: usize, // Index of first generic arg in rust
  v8_i0: usize,   // Index of first generic arg in v8/js
) -> ArgumentDecl {
  let inputs = &f.sig.inputs.iter().skip(rust_i0).enumerate();
  let ident_seq: TokenStream2 = inputs
    .clone()
    .map(|(i, _)| format!("arg_{i}"))
    .collect::<Vec<_>>()
    .join(", ")
    .parse()
    .unwrap();
  let decls: TokenStream2 = inputs
    .clone()
    .map(|(i, arg)| {
      codegen_arg(core, arg, format!("arg_{i}").as_ref(), v8_i0 + i)
    })
    .collect();
  (decls, ident_seq, inputs.len())
}

fn codegen_arg(
  core: &TokenStream2,
  arg: &syn::FnArg,
  name: &str,
  idx: usize,
) -> TokenStream2 {
  let ident = quote::format_ident!("{name}");
  let (pat, ty) = match arg {
    syn::FnArg::Typed(pat) => {
      if is_optional_fast_callback_option(&pat.ty)
        || is_optional_wasm_memory(&pat.ty)
      {
        return quote! { let #ident = None; };
      }
      (&pat.pat, &pat.ty)
    }
    _ => unreachable!(),
  };
  // Fast path if arg should be skipped
  if matches!(**pat, syn::Pat::Wild(_)) {
    return quote! { let #ident = (); };
  }
  // Fast path for `String`
  if let Some(is_ref) = is_string(&**ty) {
    let ref_block = if is_ref {
      quote! { let #ident = #ident.as_ref(); }
    } else {
      quote! {}
    };
    return quote! {
      let #ident = match #core::v8::Local::<#core::v8::String>::try_from(args.get(#idx as i32)) {
        Ok(v8_string) => #core::serde_v8::to_utf8(v8_string, scope),
        Err(_) => {
          return #core::_ops::throw_type_error(scope, format!("Expected string at position {}", #idx));
        }
      };
      #ref_block
    };
  }
  // Fast path for `Cow<'_, str>`
  if is_cow_str(&**ty) {
    return quote! {
      let #ident = match #core::v8::Local::<#core::v8::String>::try_from(args.get(#idx as i32)) {
        Ok(v8_string) => ::std::borrow::Cow::Owned(#core::serde_v8::to_utf8(v8_string, scope)),
        Err(_) => {
          return #core::_ops::throw_type_error(scope, format!("Expected string at position {}", #idx));
        }
      };
    };
  }
  // Fast path for `Option<String>`
  if is_option_string(&**ty) {
    return quote! {
      let #ident = match #core::v8::Local::<#core::v8::String>::try_from(args.get(#idx as i32)) {
        Ok(v8_string) => Some(#core::serde_v8::to_utf8(v8_string, scope)),
        Err(_) => None
      };
    };
  }
  // Fast path for &/&mut [u8] and &/&mut [u32]
  match is_ref_slice(&**ty) {
    None => {}
    Some(SliceType::U32Mut) => {
      let blck = codegen_u32_mut_slice(core, idx);
      return quote! {
        let #ident = #blck;
      };
    }
    Some(_) => {
      let blck = codegen_u8_slice(core, idx);
      return quote! {
        let #ident = #blck;
      };
    }
  }
  // Fast path for `*const u8`
  if is_ptr_u8(&**ty) {
    let blk = codegen_u8_ptr(core, idx);
    return quote! {
      let #ident = #blk;
    };
  }
  // Otherwise deserialize it via serde_v8
  quote! {
    let #ident = args.get(#idx as i32);
    let #ident = match #core::serde_v8::from_v8(scope, #ident) {
      Ok(v) => v,
      Err(err) => {
        let msg = format!("Error parsing args at position {}: {}", #idx, #core::anyhow::Error::from(err));
        return #core::_ops::throw_type_error(scope, msg);
      }
    };
  }
}

fn codegen_u8_slice(core: &TokenStream2, idx: usize) -> TokenStream2 {
  quote! {{
    let value = args.get(#idx as i32);
    match #core::v8::Local::<#core::v8::ArrayBuffer>::try_from(value) {
      Ok(b) => {
        let byte_length = b.byte_length();
        if let Some(data) = b.data() {
          let store = data.cast::<u8>().as_ptr();
          // SAFETY: rust guarantees that lifetime of slice is no longer than the call.
          unsafe { ::std::slice::from_raw_parts_mut(store, byte_length) }
        } else {
          &mut []
        }
      },
      Err(_) => {
        if let Ok(view) = #core::v8::Local::<#core::v8::ArrayBufferView>::try_from(value) {
          let len = view.byte_length();
          let offset = view.byte_offset();
          let buffer = match view.buffer(scope) {
              Some(v) => v,
              None => {
                return #core::_ops::throw_type_error(scope, format!("Expected ArrayBufferView at position {}", #idx));
              }
          };
          if let Some(data) = buffer.data() {
            let store = data.cast::<u8>().as_ptr();
            // SAFETY: rust guarantees that lifetime of slice is no longer than the call.
            unsafe { ::std::slice::from_raw_parts_mut(store.add(offset), len) }
          } else {
            &mut []
          }
        } else {
          return #core::_ops::throw_type_error(scope, format!("Expected ArrayBufferView at position {}", #idx));
        }
      }
    }}
  }
}

fn codegen_u8_ptr(core: &TokenStream2, idx: usize) -> TokenStream2 {
  quote! {{
    let value = args.get(#idx as i32);
    match #core::v8::Local::<#core::v8::ArrayBuffer>::try_from(value) {
      Ok(b) => {
        if let Some(data) = b.data() {
          data.cast::<u8>().as_ptr()
        } else {
          std::ptr::null::<u8>()
        }
      },
      Err(_) => {
        if let Ok(view) = #core::v8::Local::<#core::v8::ArrayBufferView>::try_from(value) {
          let offset = view.byte_offset();
          let buffer = match view.buffer(scope) {
              Some(v) => v,
              None => {
                return #core::_ops::throw_type_error(scope, format!("Expected ArrayBufferView at position {}", #idx));
              }
          };
          let store = if let Some(data) = buffer.data() {
            data.cast::<u8>().as_ptr()
          } else {
            std::ptr::null_mut::<u8>()
          };
          unsafe { store.add(offset) }
        } else {
          return #core::_ops::throw_type_error(scope, format!("Expected ArrayBufferView at position {}", #idx));
        }
      }
    }
  }}
}

fn codegen_u32_mut_slice(core: &TokenStream2, idx: usize) -> TokenStream2 {
  quote! {
    if let Ok(view) = #core::v8::Local::<#core::v8::Uint32Array>::try_from(args.get(#idx as i32)) {
      let (offset, len) = (view.byte_offset(), view.byte_length());
      let buffer = match view.buffer(scope) {
          Some(v) => v,
          None => {
            return #core::_ops::throw_type_error(scope, format!("Expected Uint32Array at position {}", #idx));
          }
      };
      if let Some(data) = buffer.data() {
        let store = data.cast::<u8>().as_ptr();
        // SAFETY: buffer from Uint32Array. Rust guarantees that lifetime of slice is no longer than the call.
        unsafe { ::std::slice::from_raw_parts_mut(store.add(offset) as *mut u32, len / 4) }
      } else {
        &mut []
      }
    } else {
      return #core::_ops::throw_type_error(scope, format!("Expected Uint32Array at position {}", #idx));
    }
  }
}

fn codegen_sync_ret(
  core: &TokenStream2,
  output: &syn::ReturnType,
) -> TokenStream2 {
  if is_void(output) {
    return quote! {};
  }

  if is_u32_rv(output) {
    return quote! {
      rv.set_uint32(result as u32);
    };
  }

  // Optimize Result<(), Err> to skip serde_v8 when Ok(...)
  let ok_block = if is_unit_result(output) {
    quote! {}
  } else if is_u32_rv_result(output) {
    quote! {
      rv.set_uint32(result as u32);
    }
  } else {
    quote! {
      match #core::serde_v8::to_v8(scope, result) {
        Ok(ret) => rv.set(ret),
        Err(err) => #core::_ops::throw_type_error(
          scope,
          format!("Error serializing return: {}", #core::anyhow::Error::from(err)),
        ),
      };
    }
  };

  if !is_result(output) {
    return ok_block;
  }

  quote! {
    match result {
      Ok(result) => {
        #ok_block
      },
      Err(err) => {
        let exception = #core::error::to_v8_error(scope, op_state.get_error_class_fn, &err);
        scope.throw_exception(exception);
      },
    };
  }
}

fn is_void(ty: impl ToTokens) -> bool {
  tokens(ty).is_empty()
}

fn is_result(ty: impl ToTokens) -> bool {
  let tokens = tokens(ty);
  if tokens.trim_start_matches("-> ").starts_with("Result <") {
    return true;
  }
  // Detect `io::Result<...>`, `anyhow::Result<...>`, etc...
  // i.e: Result aliases/shorthands which are unfortunately "opaque" at macro-time
  match tokens.find(":: Result <") {
    Some(idx) => !tokens.split_at(idx).0.contains('<'),
    None => false,
  }
}

fn is_string(ty: impl ToTokens) -> Option<bool> {
  let toks = tokens(ty);
  if toks == "String" {
    return Some(false);
  }
  if toks == "& str" {
    return Some(true);
  }
  None
}

fn is_option_string(ty: impl ToTokens) -> bool {
  tokens(ty) == "Option < String >"
}

fn is_cow_str(ty: impl ToTokens) -> bool {
  tokens(&ty).starts_with("Cow <") && tokens(&ty).ends_with("str >")
}

enum SliceType {
  U8,
  U8Mut,
  U32Mut,
}

fn is_ref_slice(ty: impl ToTokens) -> Option<SliceType> {
  if is_u8_slice(&ty) {
    return Some(SliceType::U8);
  }
  if is_u8_slice_mut(&ty) {
    return Some(SliceType::U8Mut);
  }
  if is_u32_slice_mut(&ty) {
    return Some(SliceType::U32Mut);
  }
  None
}

fn is_u8_slice(ty: impl ToTokens) -> bool {
  tokens(ty) == "& [u8]"
}

fn is_u8_slice_mut(ty: impl ToTokens) -> bool {
  tokens(ty) == "& mut [u8]"
}

fn is_u32_slice_mut(ty: impl ToTokens) -> bool {
  tokens(ty) == "& mut [u32]"
}

fn is_ptr_u8(ty: impl ToTokens) -> bool {
  tokens(ty) == "* const u8"
}

fn is_optional_fast_callback_option(ty: impl ToTokens) -> bool {
  tokens(&ty).contains("Option < & mut FastApiCallbackOptions")
}

fn is_optional_wasm_memory(ty: impl ToTokens) -> bool {
  tokens(&ty).contains("Option < & mut [u8]")
}

/// Detects if the type can be set using `rv.set_uint32` fast path
fn is_u32_rv(ty: impl ToTokens) -> bool {
  ["u32", "u8", "u16"].iter().any(|&s| tokens(&ty) == s) || is_resource_id(&ty)
}

/// Detects if the type is of the format Result<u32/u8/u16, Err>
fn is_u32_rv_result(ty: impl ToTokens) -> bool {
  is_result(&ty)
    && (tokens(&ty).contains("Result < u32")
      || tokens(&ty).contains("Result < u8")
      || tokens(&ty).contains("Result < u16")
      || is_resource_id(&ty))
}

/// Detects if a type is of the form Result<(), Err>
fn is_unit_result(ty: impl ToTokens) -> bool {
  is_result(&ty) && tokens(&ty).contains("Result < ()")
}

fn is_resource_id(arg: impl ToTokens) -> bool {
  static RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r#": (?:deno_core :: )?ResourceId$"#).unwrap());
  RE.is_match(&tokens(arg))
}

fn is_mut_ref_opstate(arg: impl ToTokens) -> bool {
  static RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r#": & mut (?:deno_core :: )?OpState$"#).unwrap());
  RE.is_match(&tokens(arg))
}

fn is_rc_refcell_opstate(arg: &syn::FnArg) -> bool {
  static RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r#": Rc < RefCell < (?:deno_core :: )?OpState > >$"#).unwrap()
  });
  RE.is_match(&tokens(arg))
}

fn is_handle_scope(arg: &syn::FnArg) -> bool {
  static RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r#": & mut (?:deno_core :: )?v8 :: HandleScope(?: < '\w+ >)?$"#)
      .unwrap()
  });
  RE.is_match(&tokens(arg))
}

fn is_future(ty: impl ToTokens) -> bool {
  tokens(&ty).contains("impl Future < Output =")
}

fn tokens(x: impl ToTokens) -> String {
  x.to_token_stream().to_string()
}

fn exclude_lifetime_params(
  generic_params: &Punctuated<GenericParam, Comma>,
) -> Punctuated<GenericParam, Comma> {
  generic_params
    .iter()
    .filter(|t| !tokens(t).starts_with('\''))
    .cloned()
    .collect::<Punctuated<GenericParam, Comma>>()
}

#[cfg(test)]
mod tests {
  use crate::{Attributes, Op};
  use std::path::PathBuf;

  #[testing_macros::fixture("optimizer_tests/**/*.rs")]
  fn test_codegen(input: PathBuf) {
    let update_expected = std::env::var("UPDATE_EXPECTED").is_ok();

    let source =
      std::fs::read_to_string(&input).expect("Failed to read test file");

    let mut attrs = Attributes::default();
    if source.contains("// @test-attr:fast") {
      attrs.must_be_fast = true;
    }
    if source.contains("// @test-attr:wasm") {
      attrs.is_wasm = true;
      attrs.must_be_fast = true;
    }

    let item = syn::parse_str(&source).expect("Failed to parse test file");
    let op = Op::new(item, attrs);

    let expected = std::fs::read_to_string(input.with_extension("out"))
      .expect("Failed to read expected output file");

    let actual = op.gen();
    // Validate syntax tree.
    let tree = syn::parse2(actual).unwrap();
    let actual = prettyplease::unparse(&tree);
    if update_expected {
      std::fs::write(input.with_extension("out"), actual)
        .expect("Failed to write expected file");
    } else {
      assert_eq!(actual, expected);
    }
  }
}
