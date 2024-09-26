/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under both the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree and the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree.
 */

use std::fmt;
use std::fmt::Display;

use allocative::Allocative;
use buck2_core::soft_error;
use starlark::any::ProvidesStaticType;
use starlark::coerce::Coerce;
use starlark::collections::SmallMap;
use starlark::environment::GlobalsBuilder;
use starlark::eval::Evaluator;
use starlark::starlark_complex_value;
use starlark::starlark_module;
use starlark::values::dict::Dict;
use starlark::values::dict::DictRef;
use starlark::values::dict::DictType;
use starlark::values::starlark_value;
use starlark::values::starlark_value_as_type::StarlarkValueAsType;
use starlark::values::Freeze;
use starlark::values::Freezer;
use starlark::values::FrozenValue;
use starlark::values::Heap;
use starlark::values::NoSerialize;
use starlark::values::StarlarkValue;
use starlark::values::StringValue;
use starlark::values::Trace;
use starlark::values::Tracer;
use starlark::values::UnpackValue;
use starlark::values::Value;
use starlark::values::ValueLike;
use starlark::values::ValueOf;

/// Representation of `select()` in Starlark.
#[derive(Debug, ProvidesStaticType, NoSerialize, Allocative)] // TODO selector should probably support serializing
#[repr(C)]
pub enum StarlarkSelectorGen<ValueType> {
    Inner(ValueType),
    Added(ValueType, ValueType),
}

impl<V: Display> Display for StarlarkSelectorGen<V> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            StarlarkSelectorGen::Inner(v) => {
                f.write_str("select(")?;
                v.fmt(f)?;
                f.write_str(")")
            }
            StarlarkSelectorGen::Added(l, r) => {
                l.fmt(f)?;
                f.write_str(" + ")?;
                r.fmt(f)
            }
        }
    }
}

unsafe impl<From: Coerce<To>, To> Coerce<StarlarkSelectorGen<To>> for StarlarkSelectorGen<From> {}

starlark_complex_value!(pub StarlarkSelector);

impl<'v> StarlarkSelector<'v> {
    pub fn new(d: Value<'v>) -> Self {
        StarlarkSelector::Inner(d)
    }

    fn added(left: Value<'v>, right: Value<'v>, heap: &'v Heap) -> Value<'v> {
        heap.alloc(StarlarkSelector::Added(left, right))
    }

    pub fn from_concat<I>(iter: I, heap: &'v Heap) -> Value<'v>
    where
        I: IntoIterator<Item = Value<'v>>,
    {
        fn values_to_selector<'v, I>(
            selector: Option<StarlarkSelector<'v>>,
            values: &mut I,
            heap: &'v Heap,
        ) -> Option<StarlarkSelector<'v>>
        where
            I: Iterator<Item = Value<'v>>,
        {
            match (selector, values.next()) {
                (None, None) => None,
                (None, Some(v)) => {
                    if let Some(next_v) = values.next() {
                        let head = StarlarkSelector::Added(v, next_v);
                        values_to_selector(Some(head), values, heap)
                    } else {
                        Some(StarlarkSelector::new(v))
                    }
                }
                (Some(s), None) => Some(s),
                (Some(s), Some(v)) => {
                    let head = Some(StarlarkSelector::Added(heap.alloc(s), v));
                    values_to_selector(head, values, heap)
                }
            }
        }
        let selector = values_to_selector(None, &mut iter.into_iter(), heap);
        heap.alloc(selector)
    }

    fn select_map<'a>(
        val: Value<'a>,
        eval: &mut Evaluator<'a, '_, '_>,
        func: Value<'a>,
    ) -> starlark::Result<Value<'a>> {
        fn invoke<'v>(
            eval: &mut Evaluator<'v, '_, '_>,
            func: Value<'v>,
            val: Value<'v>,
        ) -> starlark::Result<Value<'v>> {
            eval.eval_function(func, &[val], &[])
        }

        if let Some(selector) = StarlarkSelector::from_value(val) {
            match *selector {
                StarlarkSelectorGen::Inner(selector) => {
                    let selector = DictRef::from_value(selector).unwrap();
                    let mut mapped = SmallMap::with_capacity(selector.len());
                    for (k, v) in selector.iter_hashed() {
                        mapped.insert_hashed(k, invoke(eval, func, v)?);
                    }
                    Ok(eval
                        .heap()
                        .alloc(StarlarkSelector::new(eval.heap().alloc(Dict::new(mapped)))))
                }
                StarlarkSelectorGen::Added(left, right) => {
                    Ok(eval.heap().alloc(StarlarkSelectorGen::Added(
                        Self::select_map(left, eval, func)?,
                        Self::select_map(right, eval, func)?,
                    )))
                }
            }
        } else {
            invoke(eval, func, val)
        }
    }

    fn select_test<'a>(
        val: Value<'a>,
        eval: &mut Evaluator<'a, '_, '_>,
        func: Value<'a>,
    ) -> starlark::Result<bool> {
        fn invoke<'v>(
            eval: &mut Evaluator<'v, '_, '_>,
            func: Value<'v>,
            val: Value<'v>,
        ) -> starlark::Result<bool> {
            eval.eval_function(func, &[val], &[])?
                .unpack_bool()
                .ok_or_else(|| {
                    starlark::Error::new_kind(starlark::ErrorKind::Other(anyhow::anyhow!(
                        "Expected testing function to have a boolean return type"
                    )))
                })
        }

        if let Some(selector) = StarlarkSelector::from_value(val) {
            match *selector {
                StarlarkSelectorGen::Inner(selector) => {
                    let selector = DictRef::from_value(selector).unwrap();
                    for v in selector.values() {
                        let result = invoke(eval, func, v)?;
                        if result {
                            return Ok(true);
                        }
                    }
                    Ok(false)
                }
                StarlarkSelectorGen::Added(left, right) => {
                    Ok(Self::select_test(left, eval, func)?
                        || Self::select_test(right, eval, func)?)
                }
            }
        } else {
            invoke(eval, func, val)
        }
    }
}

trait StarlarkSelectorBase<'v> {
    type Item: ValueLike<'v>;
}

impl<'v> StarlarkSelectorBase<'v> for StarlarkSelector<'v> {
    type Item = Value<'v>;
}

unsafe impl<'v> Trace<'v> for StarlarkSelector<'v> {
    fn trace(&mut self, tracer: &Tracer<'v>) {
        match self {
            Self::Inner(a) => tracer.trace(a),
            Self::Added(a, b) => {
                tracer.trace(a);
                tracer.trace(b);
            }
        }
    }
}

impl<'v> Freeze for StarlarkSelector<'v> {
    type Frozen = FrozenStarlarkSelector;
    fn freeze(self, freezer: &Freezer) -> anyhow::Result<Self::Frozen> {
        Ok(match self {
            StarlarkSelector::Inner(v) => FrozenStarlarkSelector::Inner(v.freeze(freezer)?),
            StarlarkSelector::Added(l, r) => {
                FrozenStarlarkSelector::Added(l.freeze(freezer)?, r.freeze(freezer)?)
            }
        })
    }
}

impl StarlarkSelectorBase<'_> for FrozenStarlarkSelector {
    type Item = FrozenValue;
}

#[starlark_value(type = "selector")] // TODO(nga): rename to `"Select"` to match constant name.
impl<'v, V: ValueLike<'v>> StarlarkValue<'v> for StarlarkSelectorGen<V>
where
    Self: ProvidesStaticType<'v> + StarlarkSelectorBase<'v, Item = V>,
{
    fn to_bool(&self) -> bool {
        true
    }

    fn radd(&self, left: Value<'v>, heap: &'v Heap) -> Option<starlark::Result<Value<'v>>> {
        let right = heap.alloc(match self {
            StarlarkSelectorGen::Inner(x) => StarlarkSelectorGen::Inner(x.to_value()),
            StarlarkSelectorGen::Added(x, y) => {
                StarlarkSelectorGen::Added(x.to_value(), y.to_value())
            }
        });
        Some(Ok(StarlarkSelector::added(left, right, heap)))
    }

    fn add(&self, other: Value<'v>, heap: &'v Heap) -> Option<starlark::Result<Value<'v>>> {
        let this = match self {
            Self::Inner(ref v) => heap.alloc(StarlarkSelector::new(v.to_value())),
            Self::Added(ref l, ref r) => StarlarkSelector::added(l.to_value(), r.to_value(), heap),
        };

        Some(Ok(StarlarkSelector::added(this, other, heap)))
    }
}

#[starlark_module]
pub fn register_select(globals: &mut GlobalsBuilder) {
    const Select: StarlarkValueAsType<StarlarkSelector> = StarlarkValueAsType::new();

    fn select<'v>(#[starlark(require = pos)] d: Value<'v>) -> anyhow::Result<StarlarkSelector<'v>> {
        if let Err(e) = ValueOf::<DictType<StringValue, Value>>::unpack_value_err(d) {
            soft_error!("select_not_dict", e.into())?;
        }
        Ok(StarlarkSelector::new(d))
    }

    /// Maps a selector.
    ///
    /// Each value within a selector map and on each side of an addition will be passed to the
    /// mapping function. The returned selector will have the same structure as this one.
    ///
    /// Ex:
    /// ```starlark
    /// def increment_items(a):
    ///     return [v + 1 for v in a]
    ///
    /// select_map([1, 2] + select({"c": [2]}), increment_items) == [2, 3] + select({"c": [3]})
    /// ```
    fn select_map<'v>(
        #[starlark(require = pos)] d: Value<'v>,
        #[starlark(require = pos)] func: Value<'v>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<Value<'v>> {
        StarlarkSelector::select_map(d, eval, func)
    }

    /// Test values in the select expression using the given function.
    ///
    /// Returns True, if any value in the select passes, else False.
    ///
    /// Ex:
    /// ```starlark
    /// select_test([1] + select({"c": [1]}), lambda a: len(a) > 1) == False
    /// select_test([1, 2] + select({"c": [1]}), lambda a: len(a) > 1) == True
    /// select_test([1] + select({"c": [1, 2]}), lambda a: len(a) > 1) == True
    /// ```
    fn select_test<'v>(
        #[starlark(require = pos)] d: Value<'v>,
        #[starlark(require = pos)] func: Value<'v>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<bool> {
        StarlarkSelector::select_test(d, eval, func)
    }

    /// Tests that two selects are equal to each other. For testing use only.
    /// We simply compare their string representations.
    fn select_equal_internal<'v>(
        #[starlark(require = pos)] left: Value<'v>,
        #[starlark(require = pos)] right: Value<'v>,
    ) -> anyhow::Result<bool> {
        Ok(left.to_repr() == right.to_repr())
    }
}
