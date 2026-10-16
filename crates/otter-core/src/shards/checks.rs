//! Checks and type changes (docs/host-requirements.md §3): `IsTrue`,
//! `IsFalse`, `Pass`, `ToAny`, `ToAnyTable`, the sequence forms of
//! `Expect` (`ExpectIntSeq`, ...), `Assert.Is` and `Assert.IsNot`. All leaf
//! shards, all on the compose-time list (`compose_time.rs`): their work is
//! constant or charged by size.

use super::leaf::{LeafShard, leaf_type};
use super::values::{Pure, PureOp, comparable, fail, one_of, pure_desc, values_equal};
use super::*;
use crate::compose_time::Meter;
use crate::instance::{InstanceCtx, LeafCtx};
use crate::shard::Flow;

pub static IS_TRUE: ShardType = leaf_type::<Pure<BoolTest<true>>>();
pub static IS_FALSE: ShardType = leaf_type::<Pure<BoolTest<false>>>();
pub static PASS: ShardType = leaf_type::<Pass>();
pub static TO_ANY: ShardType = leaf_type::<Pure<ToAnyOp>>();
pub static TO_ANY_TABLE: ShardType = leaf_type::<Pure<ToAnyTableOp>>();
pub static EXPECT_INT_SEQ: ShardType = leaf_type::<Pure<ExpectSeqOf<IntSeqK>>>();
pub static EXPECT_FLOAT_SEQ: ShardType = leaf_type::<Pure<ExpectSeqOf<FloatSeqK>>>();
pub static EXPECT_BOOL_SEQ: ShardType = leaf_type::<Pure<ExpectSeqOf<BoolSeqK>>>();
pub static EXPECT_STRING_SEQ: ShardType = leaf_type::<Pure<ExpectSeqOf<StringSeqK>>>();
pub static EXPECT_BYTES_SEQ: ShardType = leaf_type::<Pure<ExpectSeqOf<BytesSeqK>>>();
pub static EXPECT_TABLE_SEQ: ShardType = leaf_type::<Pure<ExpectSeqOf<TableSeqK>>>();
pub static EXPECT_FLOAT2_SEQ: ShardType = leaf_type::<Pure<ExpectSeqOf<Float2SeqK>>>();
pub static EXPECT_FLOAT3_SEQ: ShardType = leaf_type::<Pure<ExpectSeqOf<Float3SeqK>>>();
pub static EXPECT_FLOAT4_SEQ: ShardType = leaf_type::<Pure<ExpectSeqOf<Float4SeqK>>>();
pub static ASSERT_IS: ShardType = leaf_type::<Assert<true>>();
pub static ASSERT_IS_NOT: ShardType = leaf_type::<Assert<false>>();

const BOOL: &[TypeName] = &[TypeName::Bool];

// --- IsTrue, IsFalse ---

/// `IsTrue` (`WANT` true) and `IsFalse`.
pub struct BoolTest<const WANT: bool>;
impl<const WANT: bool> PureOp for BoolTest<WANT> {
  const DESC: ShardDesc = pure_desc(
    if WANT { "IsTrue" } else { "IsFalse" },
    if WANT {
      crate::shard_doc!("Outputs whether the input Bool is true.")
    } else {
      crate::shard_doc!("Outputs whether the input Bool is false (the same as `Not`).")
    },
    "",
    InputDesc::Types(BOOL),
    OutputDesc::Fixed(TypeName::Bool),
  );
  fn output(input: Type) -> std::result::Result<Type, &'static [TypeName]> {
    one_of(input, BOOL, Type::bool())
  }
  fn apply(input: &Var) -> Result<Var> {
    match input {
      Var::Bool(b) => Ok(Var::Bool(*b == WANT)),
      _ => Err(fail(Self::DESC.name, "input is not a Bool")),
    }
  }
}

// --- Pass ---

pub struct Pass;
impl LeafShard for Pass {
  type Compiled = ();
  type State = ();
  const DESC: ShardDesc = pure_desc(
    "Pass",
    crate::shard_doc!("Does nothing: passes its input through."),
    crate::shard_doc!(
      "A placeholder where a flow is needed but nothing should happen (`{}` is the empty flow)."
    ),
    InputDesc::Any,
    OutputDesc::Passthrough,
  );
  fn compose(_: &Args, ctx: &mut ComposeCtx<'_>) -> Result<Composed<()>> {
    Ok(Composed {
      compiled: (),
      output: ctx.input(),
    })
  }
  fn instantiate(_: &(), _: &mut InstanceCtx) -> Result<()> {
    Ok(())
  }
  fn activate(_: &(), _: &mut (), _: &mut impl LeafCtx, input: &Var) -> Result<Flow> {
    Ok(Flow::Next(input.clone()))
  }
}

// --- ToAny, ToAnyTable ---

pub struct ToAnyOp;
impl PureOp for ToAnyOp {
  const DESC: ShardDesc = pure_desc(
    "ToAny",
    crate::shard_doc!("Passes its input through, typed as Any."),
    crate::shard_doc!(
      "Forgets the input's type, so the value can go where any type is accepted: `none | ToAny | Var(result)` declares a variable that later takes values of any type. Reading an Any value where a type is expected needs an `Expect` shard first."
    ),
    InputDesc::Any,
    OutputDesc::Fixed(TypeName::Any),
  );
  fn output(_: Type) -> std::result::Result<Type, &'static [TypeName]> {
    Ok(Type::any())
  }
  fn apply(input: &Var) -> Result<Var> {
    Ok(input.clone())
  }
}

const TABLE: &[TypeName] = &[TypeName::Table];

pub struct ToAnyTableOp;
impl PureOp for ToAnyTableOp {
  const DESC: ShardDesc = pure_desc(
    "ToAnyTable",
    crate::shard_doc!("Passes a table through, typed as a table of any keys and values."),
    crate::shard_doc!(
      "Forgets which keys the table's type lists: `{a: 1} | ToAnyTable | Var(t)` declares a variable that later takes any table, where `{a: 1} | Var(t)` takes only tables with an Int `a`. Reading a key then gives an Any value or none."
    ),
    InputDesc::Types(TABLE),
    OutputDesc::Fixed(TypeName::Table),
  );
  fn output(input: Type) -> std::result::Result<Type, &'static [TypeName]> {
    one_of(input, TABLE, Type::any_table())
  }
  fn apply(input: &Var) -> Result<Var> {
    match input {
      Var::Table(_) => Ok(input.clone()),
      _ => Err(fail("ToAnyTable", "input is not a table")),
    }
  }
}

// --- ExpectIntSeq, ExpectFloatSeq, ... ---

/// The element type an `Expect*Seq` shard narrows a sequence to.
pub trait SeqKind: 'static {
  const NAME: &'static str;
  const ELEMENT: TypeName;
}

macro_rules! seq_kind {
  ($kind:ident, $name:literal, $element:ident) => {
    pub struct $kind;
    impl SeqKind for $kind {
      const NAME: &'static str = $name;
      const ELEMENT: TypeName = TypeName::$element;
    }
  };
}

seq_kind!(IntSeqK, "ExpectIntSeq", Int);
seq_kind!(FloatSeqK, "ExpectFloatSeq", Float);
seq_kind!(BoolSeqK, "ExpectBoolSeq", Bool);
seq_kind!(StringSeqK, "ExpectStringSeq", String);
seq_kind!(BytesSeqK, "ExpectBytesSeq", Bytes);
seq_kind!(TableSeqK, "ExpectTableSeq", Table);
seq_kind!(Float2SeqK, "ExpectFloat2Seq", Float2);
seq_kind!(Float3SeqK, "ExpectFloat3Seq", Float3);
seq_kind!(Float4SeqK, "ExpectFloat4Seq", Float4);

/// Whether a value of type `actual` may be of type `target` at run time:
/// `target` accepts it, or it is Any, or a union with such a member, or a
/// sequence whose element may be.
fn may_be(actual: Type, target: Type) -> bool {
  if target.accepts(actual) {
    return true;
  }
  match (actual.desc(), target.desc()) {
    (TypeDesc::Any, _) => true,
    (TypeDesc::Union(members), _) => members.iter().any(|m| may_be(*m, target)),
    (TypeDesc::Seq(a), TypeDesc::Seq(t)) => may_be(*a, *t),
    _ => false,
  }
}

pub struct ExpectSeqOf<K>(std::marker::PhantomData<K>);
impl<K: SeqKind> ExpectSeqOf<K> {
  fn target() -> Type {
    Type::seq(K::ELEMENT.to_type())
  }
}
impl<K: SeqKind> PureOp for ExpectSeqOf<K> {
  const DESC: ShardDesc = pure_desc(
    K::NAME,
    crate::shard_doc!(
      "Narrows the input to a sequence of one element type, failing at activation if it is not one."
    ),
    crate::shard_doc!(
      "The input's type must be able to hold such a sequence: Any, a sequence whose element type can hold the element (`[Any]`, `[Int | String]`), or a union with such a member. Every element is checked at activation; a value of another type, or an element of another type, is an activation error."
    ),
    InputDesc::Any,
    OutputDesc::Dynamic(crate::shard_doc!("the expected sequence type")),
  );
  fn output(input: Type) -> std::result::Result<Type, &'static [TypeName]> {
    let target = Self::target();
    if target.accepts(input) {
      Ok(input)
    } else if may_be(input, target) {
      Ok(target)
    } else {
      Err(&[TypeName::Seq])
    }
  }
  fn meter(input: &Var, meter: &Meter) -> Result<()> {
    meter.charge(input.as_seq().map_or(0, <[Var]>::len) as u64)
  }
  fn apply(input: &Var) -> Result<Var> {
    let Some(items) = input.as_seq() else {
      return Err(fail(
        K::NAME,
        &format!("expected a sequence, got {}", input.type_of()),
      ));
    };
    let element = K::ELEMENT.to_type();
    for (i, item) in items.iter().enumerate() {
      if !element.admits(item) {
        return Err(fail(
          K::NAME,
          &format!(
            "element {i} is {}, not {}",
            item.type_of(),
            K::ELEMENT.name()
          ),
        ));
      }
    }
    Ok(input.clone())
  }
}

// --- Assert.Is, Assert.IsNot ---

static ASSERT_PARAMS: &[ParamDecl] = &[decl(
  "value",
  crate::shard_doc!("The value to compare with: a literal, or a variable read at activation."),
  OPERAND,
  NONE_TYPES,
  Requirement::Required,
)];

/// `Assert.Is` (`EQUAL` true) and `Assert.IsNot`.
pub struct Assert<const EQUAL: bool>;
impl<const EQUAL: bool> Assert<EQUAL> {
  const NAME: &'static str = if EQUAL { "Assert.Is" } else { "Assert.IsNot" };
}
impl<const EQUAL: bool> LeafShard for Assert<EQUAL> {
  type Compiled = Operand;
  type State = ();
  const DESC: ShardDesc = ShardDesc {
    name: Self::NAME,
    version: 1,
    summary: if EQUAL {
      crate::shard_doc!("Fails unless the input equals a value; passes the input through.")
    } else {
      crate::shard_doc!("Fails if the input equals a value; passes the input through.")
    },
    help: crate::shard_doc!(
      "Values compare as in `Is` (numbers by value, sequences and tables element by element), and the types must be able to match (a compose error otherwise). A failed assertion is an activation error naming both values."
    ),
    params: Params::Declared(ASSERT_PARAMS),
    input: InputDesc::Any,
    output: OutputDesc::Passthrough,
    targets: Targets::All,
    aliases: &[],
    effects: crate::signature::Effects::NONE,
    lifetime: crate::signature::Lifetime::Stateless,
  };

  fn compose(args: &Args, ctx: &mut ComposeCtx<'_>) -> Result<Composed<Operand>> {
    let (operand, ty) = Operand::compose_arg(args, "value", Self::NAME, ctx)?;
    let input = ctx.input();
    if !comparable(input, ty) {
      return Err(Error::Diagnostic(Box::new(
        Diagnostic::new(
          Phase::Compose,
          "input-type-mismatch",
          "input-type-mismatch",
          format!("{input} and {ty} can never be equal"),
        )
        .shard(Self::NAME)
        .param("value", Some(0))
        .types(Some(TypeRef::of(input)), vec![TypeRef::of(ty)]),
      )));
    }
    Ok(Composed {
      compiled: operand,
      output: input,
    })
  }

  fn instantiate(_: &Operand, _: &mut InstanceCtx) -> Result<()> {
    Ok(())
  }

  fn activate(op: &Operand, _: &mut (), ctx: &mut impl LeafCtx, input: &Var) -> Result<Flow> {
    let value = op.get(ctx);
    if let Some(meter) = ctx.meter() {
      meter.traverse(input)?;
      meter.traverse(&value)?;
    }
    if values_equal(input, &value) == EQUAL {
      return Ok(Flow::Next(input.clone()));
    }
    Err(fail(
      Self::NAME,
      &if EQUAL {
        format!("expected {value}, got {input}")
      } else {
        format!("expected anything but {value}")
      },
    ))
  }
}
