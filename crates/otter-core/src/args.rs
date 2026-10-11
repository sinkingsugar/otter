//! Shared argument decoding (docs/shard-metadata-and-compose.md §4).
//!
//! [`decode`] maps a shard definition's arguments onto the shard's declared
//! parameters ([`crate::describe::ParamDecl`], the same declarations the
//! catalog documents): positional and named arguments, unknown, duplicate
//! and missing arguments, defaults, accepted forms and literal types. It
//! keeps variable references unresolved: compose resolves them through
//! `ComposeCtx`, so bindings, types and dependencies stay in the existing
//! checking and caching model. Shard compose reads the result through the
//! checked accessors of [`Args`].

use crate::describe::{Forms, ParamDecl, Params, Requirement, ShardDesc, check_params};
use crate::diagnostic::{Diagnostic, Phase, TypeRef};
use crate::error::{Error, Result};
use crate::shard::{ParamValue, ShardDef};
use crate::var::Var;

/// One argument of a shard definition: positional, or named.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Arg {
  pub name: Option<String>,
  pub value: ParamValue,
}

impl Arg {
  pub fn pos(value: ParamValue) -> Arg {
    Arg { name: None, value }
  }

  pub fn named(name: &str, value: ParamValue) -> Arg {
    Arg {
      name: Some(name.to_string()),
      value,
    }
  }
}

/// Decoded arguments, as a shard's compose receives them.
pub struct Args {
  inner: Inner,
}

enum Inner {
  /// An undescribed shard: its positional arguments, unchanged.
  Positional(Vec<ParamValue>),
  /// A described shard: one entry per declared parameter, in declaration
  /// order (`None`: an optional parameter that was not given).
  Decoded {
    decls: &'static [ParamDecl],
    values: Vec<Option<ParamValue>>,
    /// The arguments of a variadic last parameter, in order.
    rest: Vec<ParamValue>,
  },
}

impl Args {
  /// The positional arguments of a shard that does not declare its
  /// parameters yet. Panics for a described shard, which must use the
  /// named accessors.
  pub fn positional(&self) -> &[ParamValue] {
    match &self.inner {
      Inner::Positional(values) => values,
      Inner::Decoded { .. } => panic!("described shard must read its arguments by name"),
    }
  }

  fn index(&self, name: &str) -> usize {
    match &self.inner {
      Inner::Decoded { decls, .. } => decls
        .iter()
        .position(|d| d.name == name)
        .unwrap_or_else(|| panic!("parameter {name} is not declared")),
      Inner::Positional(_) => panic!("undescribed shard has no named parameters"),
    }
  }

  /// The argument for a declared parameter, after defaults. `None` only for
  /// an optional parameter that was not given. Panics if `name` is not a
  /// declared parameter (a bug in the shard).
  pub fn get(&self, name: &str) -> Option<&ParamValue> {
    let index = self.index(name);
    match &self.inner {
      Inner::Decoded { values, .. } => values[index].as_ref(),
      Inner::Positional(_) => unreachable!(),
    }
  }

  /// The arguments of the variadic parameter `name` (possibly none).
  /// Panics if `name` is not the declared variadic parameter.
  pub fn variadic(&self, name: &str) -> &[ParamValue] {
    let index = self.index(name);
    match &self.inner {
      Inner::Decoded { decls, rest, .. } => {
        assert!(
          decls[index].requirement == Requirement::Variadic,
          "parameter {name} is not variadic"
        );
        rest
      }
      Inner::Positional(_) => unreachable!(),
    }
  }

  /// Position of a declared parameter, for diagnostics.
  pub fn param_index(&self, name: &str) -> usize {
    self.index(name)
  }

  /// The declaration of a parameter (`None` for an undescribed shard).
  pub fn decl(&self, name: &str) -> Option<&'static ParamDecl> {
    match &self.inner {
      Inner::Decoded { decls, .. } => decls.iter().find(|d| d.name == name),
      Inner::Positional(_) => None,
    }
  }

  pub fn literal(&self, name: &str) -> Option<&Var> {
    match self.get(name)? {
      ParamValue::Value(v) => Some(v),
      _ => None,
    }
  }

  pub fn int(&self, name: &str) -> Option<i64> {
    match self.literal(name)? {
      Var::Int(i) => Some(*i),
      _ => None,
    }
  }

  /// A variable reference argument: the variable's name.
  pub fn variable(&self, name: &str) -> Option<&str> {
    match self.get(name)? {
      ParamValue::Var(var) => Some(var),
      _ => None,
    }
  }

  pub fn bool(&self, name: &str) -> Option<bool> {
    match self.literal(name)? {
      Var::Bool(b) => Some(*b),
      _ => None,
    }
  }

  pub fn string(&self, name: &str) -> Option<&str> {
    match self.literal(name)? {
      Var::String(s) => Some(s),
      _ => None,
    }
  }

  pub fn flow(&self, name: &str) -> Option<&[ShardDef]> {
    match self.get(name)? {
      ParamValue::Flow(flow) => Some(flow),
      _ => None,
    }
  }

  pub fn wire(&self, name: &str) -> Option<&str> {
    match self.get(name)? {
      ParamValue::Wire(wire) => Some(wire),
      _ => None,
    }
  }
}

impl Args {
  /// The declared parameter that holds this nested flow (by identity) or
  /// references this wire, for diagnostic paths.
  /// For a flow inside `cases`, also the case's index.
  pub(crate) fn param_of(
    &self,
    child: &crate::compose::Child,
  ) -> Option<(&'static str, Option<usize>)> {
    let Inner::Decoded {
      decls,
      values,
      rest,
    } = &self.inner
    else {
      return None;
    };
    let matches = |value: &ParamValue| -> Option<Option<usize>> {
      match (child, value) {
        (crate::compose::Child::Flow(ptr), ParamValue::Flow(flow)) => {
          std::ptr::eq(flow.as_ptr(), *ptr).then_some(None)
        }
        (crate::compose::Child::Flow(ptr), ParamValue::Cases(cases)) => cases
          .iter()
          .position(|(_, flow)| std::ptr::eq(flow.as_ptr(), *ptr))
          .map(Some),
        (crate::compose::Child::Wire(name), ParamValue::Wire(wire)) => {
          (wire == name).then_some(None)
        }
        _ => None,
      }
    };
    let declared = decls
      .iter()
      .zip(values)
      .find_map(|(decl, value)| matches(value.as_ref()?).map(|case| (decl.name, case)));
    // A flow among variadic arguments (`All`, `Any`): the argument's index.
    declared.or_else(|| {
      let decl = decls.last()?;
      rest
        .iter()
        .enumerate()
        .find_map(|(i, value)| matches(value).map(|_| (decl.name, Some(i))))
    })
  }

  /// The value-flow pairs of a `cases` parameter.
  pub fn cases(&self, name: &str) -> Option<&[(Var, Vec<ShardDef>)]> {
    match self.get(name)? {
      ParamValue::Cases(cases) => Some(cases),
      _ => None,
    }
  }
}

/// The accepted types of a declaration, for diagnostics.
pub(crate) fn expected_refs(decl: &ParamDecl) -> Vec<TypeRef> {
  match decl.ty {
    Some(ty) => vec![TypeRef::of(ty())],
    None => decl.types.iter().copied().map(TypeRef::named).collect(),
  }
}

fn form_of(value: &ParamValue) -> (Forms, &'static str) {
  match value {
    ParamValue::Value(_) => (Forms::LITERAL, "literal"),
    ParamValue::Var(_) => (Forms::VARIABLE, "variable"),
    ParamValue::Wire(_) => (Forms::WIRE, "wire"),
    ParamValue::Flow(_) => (Forms::FLOW, "flow"),
    ParamValue::Cases(_) => (Forms::CASES, "cases"),
    // Compose evaluates it before decoding; a literal once it has.
    ParamValue::Eval(_) => (Forms::LITERAL, "literal"),
  }
}

fn construct_error(desc: &ShardDesc, code: &'static str, message: String) -> Diagnostic {
  Diagnostic::new(Phase::Construct, "generic", code, message).shard(desc.name)
}

/// Decodes `args` against `desc`'s parameter declarations.
pub fn decode(desc: &ShardDesc, args: &[Arg]) -> Result<Args> {
  let decls = match desc.params {
    Params::Undeclared => {
      if let Some(arg) = args.iter().find(|a| a.name.is_some()) {
        let name = arg.name.as_deref().unwrap_or_default();
        return Err(Error::Diagnostic(Box::new(
          construct_error(
            desc,
            "named-arguments-unsupported",
            format!("{} does not take named arguments (got {name})", desc.name),
          )
          .param(name, None),
        )));
      }
      return Ok(Args {
        inner: Inner::Positional(args.iter().map(|a| a.value.clone()).collect()),
      });
    }
    Params::Declared(decls) => decls,
  };
  // The same check `ShardType::new` runs at compile time, for descriptions
  // used directly: defaults must follow the declared contract.
  if let Err(index) = check_params(decls) {
    return Err(Error::Diagnostic(Box::new(
      construct_error(
        desc,
        "invalid-declaration",
        format!(
          "{}: parameter {} is declared with a duplicate name or a default it does not accept",
          desc.name, decls[index].name
        ),
      )
      .param(decls[index].name, Some(index)),
    )));
  }

  let fail = |d: Diagnostic| Err(Error::Diagnostic(Box::new(d)));
  let mut values: Vec<Option<ParamValue>> = vec![None; decls.len()];
  let mut rest: Vec<ParamValue> = Vec::new();
  let variadic = decls
    .last()
    .filter(|d| d.requirement == Requirement::Variadic)
    .map(|_| decls.len() - 1);
  let mut seen_named = false;
  for (position, arg) in args.iter().enumerate() {
    let index = match &arg.name {
      None => {
        if seen_named {
          return fail(construct_error(
            desc,
            "positional-after-named",
            format!(
              "{}: positional argument {} after a named one",
              desc.name, position
            ),
          ));
        }
        match variadic {
          Some(v) if position >= v => v,
          _ if position >= decls.len() => {
            return fail(construct_error(
              desc,
              "too-many-arguments",
              format!(
                "{} takes at most {} arguments, got {}",
                desc.name,
                decls.len(),
                args.len()
              ),
            ));
          }
          _ => position,
        }
      }
      Some(name) => {
        seen_named = true;
        match decls.iter().position(|d| d.name == name) {
          Some(index) if Some(index) == variadic => {
            return fail(
              construct_error(
                desc,
                "variadic-by-name",
                format!(
                  "{}: {name} takes the remaining positional arguments; it cannot be given by name",
                  desc.name
                ),
              )
              .param(name, Some(index)),
            );
          }
          Some(index) => index,
          None => {
            let known: Vec<&str> = decls.iter().map(|d| d.name).collect();
            return fail(
              construct_error(
                desc,
                "unknown-argument",
                format!(
                  "{} has no parameter {name} (parameters: {})",
                  desc.name,
                  known.join(", ")
                ),
              )
              .param(name, None),
            );
          }
        }
      }
    };
    let decl = &decls[index];
    if values[index].is_some() && Some(index) != variadic {
      return fail(
        construct_error(
          desc,
          "duplicate-argument",
          format!("{}: {} given more than once", desc.name, decl.name),
        )
        .param(decl.name, Some(index)),
      );
    }
    let (form, form_name) = form_of(&arg.value);
    if !decl.forms.contains(form) {
      return fail(
        construct_error(
          desc,
          "wrong-argument-form",
          format!(
            "{}: {} takes a {}, got a {form_name}",
            desc.name,
            decl.name,
            decl.forms.names().join(" or ")
          ),
        )
        .param(decl.name, Some(index)),
      );
    }
    if let ParamValue::Value(v) = &arg.value {
      let ty = v.type_of();
      if !decl.accepts(ty) {
        return fail(
          construct_error(
            desc,
            "wrong-argument-type",
            format!(
              "{}: {} must be {}, got {ty}",
              desc.name,
              decl.name,
              decl.expected()
            ),
          )
          .param(decl.name, Some(index))
          .types(Some(TypeRef::of(ty)), expected_refs(decl)),
        );
      }
    }
    // Literal tables enter compose as struct tables (golden path §7.3).
    let value = match &arg.value {
      ParamValue::Value(v) => ParamValue::Value(v.clone().into_struct_tables()),
      other => other.clone(),
    };
    if Some(index) == variadic {
      rest.push(value);
    } else {
      values[index] = Some(value);
    }
  }

  for (index, decl) in decls.iter().enumerate() {
    if values[index].is_none() {
      match decl.requirement {
        Requirement::Required => {
          return fail(
            construct_error(
              desc,
              "missing-argument",
              format!("{}: missing required parameter {}", desc.name, decl.name),
            )
            .param(decl.name, Some(index)),
          );
        }
        Requirement::Default(default) => {
          values[index] = Some(ParamValue::Value(default.to_var().into_struct_tables()));
        }
        Requirement::Optional | Requirement::Variadic => {}
      }
    }
  }
  Ok(Args {
    inner: Inner::Decoded {
      decls,
      values,
      rest,
    },
  })
}
