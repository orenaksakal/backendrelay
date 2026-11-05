# Vendored `cel` patches

This directory is the crates.io release `cel` 0.14.3, vendored under its MIT
license (see `LICENSE`). The upstream archive checksum is
`f82399e72d100a28a0cac14251a8aba16d124af2a237d8a60ab7997ba7380cc3`.

Instant's permission oracle is `cel-java` 0.11.1. The upstream crate hard-codes
equality, ordering, membership and map-key lookup before environment overload
dispatch, so these differences cannot be corrected by registering functions
from `instant-permissions`. They are patched here instead.

`server-rs/Cargo.toml` must keep `cel` patched through `[patch.crates-io]`:

```toml
[patch.crates-io]
cel = { path = "vendor/cel" }
```

Naively changing the registry version, or letting a tool rewrite the patch
section, silently replaces these semantics with upstream behavior. Every
behavioral change must also have a covering test; add an entry here when you
add one.

## Recreating the diff

From the repository root, with network access:

```sh
tmp="$(mktemp -d)"
curl -fsSL https://crates.io/api/v1/crates/cel/0.14.3/download | tar -xz -C "$tmp"
diff -ru "$tmp/cel-0.14.3/src" server-rs/vendor/cel/src
rm -rf "$tmp"
```

Packaging files match the archive (`Cargo.toml` and `Cargo.toml.orig` are
byte-identical). The only non-source differences are `.cargo-ok` and `LICENSE`
present here, and `README.md`/`CHANGELOG.md` omitted. All behavioral changes
are under `src/`.

## Running the tests

From `server-rs/`:

```sh
env -u RUSTUP_TOOLCHAIN cargo test --release --locked \
  --manifest-path vendor/cel/Cargo.toml --features json
```

Release mode is required: the deep-nesting parser test overflows the default
2 MiB test-thread stack in unoptimised builds. The same command is a gate in
`self-hosting/verify-local.py`, and CI runs it.

The `instant-permissions` tests below execute rules through this crate, so they
also fail if the patch disappears.

## 1. Java/CEL numeric comparison

**Files:** `src/common/types/mod.rs` (`java_double_compare`,
`compare_double_int`, `compare_double_uint`, `compare_int_uint`),
`src/common/types/double.rs` (`Comparer`, `Val::equals`),
`src/common/types/int.rs` (`Comparer`), `src/common/types/uint.rs`
(`Comparer`).

**Change:** int, uint and double ordering call the four helpers in
`common::types` that mirror cel-java's comparison functions: int/uint ordering
uses the oracle's `i64::MAX` boundary rule, int/uint vs double ordering guards
the exact double range before Java `Double.compare`, and double ordering is
Java `Double.compare` (`NaN` after every finite value, `-0.0` before `+0.0`).
Double equality is primitive CEL equality, so `0.0 == -0.0` but `NaN != NaN`.

**Why:** upstream's int/uint ordering falls back to a blanket `Less`/`Greater`
after `try_into`, and every `NaN` ordering returns `NoSuchOverload`.
cel-java's `ComparisonFunctions` applies the pinned numeric boundary rules and
orders `NaN` as a total order with Java `Double.compare`. Rules such as
`auth.value > 9007199254740992`, `auth.value == -0.0` and
`9223372036854775807 < 9223372036854775807u` must match the oracle before
query pushdown.

**Tests:**
`server-rs/crates/instant-permissions/src/rule_where.rs`
`concrete_numeric_decisions_match_cel_runtime_before_query_pushdown` (runtime
and pushdown agreement for 2^53 and `-0.0` boundaries), and
`server-rs/vendor/cel/src/objects.rs` `test_float_compare` (NaN equality and
ordering).

## 2. Lossless numeric map keys

**Files:** `src/common/types/map.rs` (`DefaultMap::find_key`,
`DefaultMap::find_value`, `double_to_i64_lossless`, `double_to_u64_lossless`,
`key_string`, `Val::equals`, `Container::contains`, `Indexer::get`,
`Indexer::steal`, plus the `tests` module).

**Change:** map lookup first tries the exact native key, then retries only
lossless numeric representations: non-negative ints through uint, in-range
uints through int, and integral doubles through int and uint. Fractional,
`NaN` and infinite doubles are absent keys. Double-to-uint retries stop below
2^63 to mirror cel-java's `UnsignedLong.longValue()` high-bit behavior. Map
equality compares resolved keys and values, and index/steal distinguish a
missing supported key (`NoSuchKey`) from an unsupported key type
(`NoSuchOverload`).

**Why:** CEL map literals may mix int and uint keys, and `dyn()` values can be
indexed into either. Upstream key hashing only matched identical variants, so
`{1: 'v'} == {1u: 'v'}`, `dyn(1) in {1u: true}` and `{1u: true}[dyn(1)]`
diverged from cel-java.

**Tests:**
`server-rs/vendor/cel/src/common/types/map.rs`
`indexes_negative_integral_double_as_int`,
`indexes_both_signed_double_zeroes_as_int_zero`,
`indexes_integer_boundaries_without_lossy_double_coercion`,
`rejects_fractional_and_non_finite_double_keys`,
`distinguishes_missing_supported_keys_from_unsupported_index_types`;
`server-rs/crates/instant-permissions/src/lib.rs`
`numeric_map_keys_match_cel_java_lossless_lookup_semantics` and
`ruleset_post_filter_uses_lossless_numeric_map_keys`.

## 3. Java-native list containment

**Files:** `src/common/types/list.rs` (`Container::contains`),
`src/common/types/mod.rs` (`java_object_equals`).

**Change:** the first containment pass uses Java `Object.equals` semantics for
the element: equal kinds only, doubles compared by bit pattern with
`NaN == NaN`, and nested lists/maps compared recursively. Only when the
searched value is numeric does a second pass fall back to CEL numeric
equality.

**Why:** cel-java's `in` implements `List.contains` with `Object.equals`, and
retries with CEL numeric equality only for numeric search values. Upstream
called CEL `equals` for every element, so `[1] in [[1.0]]` wrongly returned
true and NaN/`-0.0` identity differed from the oracle.

**Tests:**
`server-rs/crates/instant-permissions/src/rule_where.rs`
`concrete_numeric_decisions_match_cel_runtime_before_query_pushdown` includes
`auth.value in [[1.0]]` (must be false) and `auth.value in [1.0, 2.0]` (numeric
retry); `server-rs/crates/instant-permissions/src/lib.rs`
`collection_operators_evaluate_with_cel_java_semantics`.

## 4. `type()` and type-value bindings

**Files:** `src/common/types/mod.rs` (`TYPE_VALUE_TYPE`, `impl Val for Type`,
`type_of`, `stdlib`), `src/env.rs` (`Env::stdlib` registers
`types::stdlib`).

**Change:** `Type` implements `Val` (kind `type`, equality by type, boxed
clone), the `type(type) -> type` overload returns `args[0].get_type()`, and
`Env::stdlib()` registers it.

**Why:** permission rules use `type(data.rank) == int`, and cel-java declares
`type(A) -> type(A)` together with the standard type-denotation identifiers
(`int`, `uint`, `list`, ...) as values. Upstream has neither, so those rules do
not even compile.

**Tests:** `server-rs/vendor/cel/src/context.rs`
`standard_type_bindings_are_immutable_fallbacks_with_local_override_priority`
and `server-rs/crates/instant-permissions/src/lib.rs`
`type_operator_and_standard_identifiers_match_cel_java`.

## 5. Shared standard environment and function scopes

**Files:** `src/context.rs` (`STANDARD_ENV`, `STANDARD_VARIABLES`,
`standard_variables`, `function_parent`, `get_function`, `new_function_scope`).

**Change:** default contexts share one lazily built `Env::stdlib()` and one
immutable set of the 13 standard type bindings; a context's own variables
still take precedence. `new_function_scope()` creates a fresh default root
that inherits only native function definitions from its parent, not variables,
resolvers, or work budgets.

**Why:** every permission evaluation previously built a default `Context`,
rebuilding `Env::stdlib()` and boxing all type bindings per entity, which showed
up as allocation-heavy hot permission work under load.
`instant-permissions` shares a `PERMISSION_FUNCTIONS` context and derives one
scope per evaluation, so concurrent request variables, resolvers and fuel must
stay isolated while native definitions are reused.

**Tests:** `server-rs/vendor/cel/src/context.rs`
`default_contexts_share_standard_declarations_not_bindings_functions_or_fuel`
and
`function_scopes_share_definitions_not_variables_overrides_resolvers_or_fuel`;
`server-rs/crates/instant-permissions/tests/evaluation_context_isolation.rs`
`shared_function_definitions_keep_concurrent_subjects_and_decisions_fresh`.

## 6. Work limit (fuel)

**Files:** `src/lib.rs` (`ExecutionError::WorkLimitExceeded`),
`src/context.rs` (`work_remaining`, `work_exhausted`, `set_work_limit`,
`work_remaining`, `consume_work`, `consume_work_units`, `exhaust_work`,
`check_work_limit`), `src/objects.rs` (`resolve_val` wrapper).

**Change:** each evaluation root owns an atomic fuel budget. Every expression
visit costs one unit, every sized value costs `ceil(size / 64)` units, and
child scopes delegate to the root. A nested `WorkLimitExceeded` marks the
budget terminally exhausted so a native function cannot swallow it, and
evaluation re-checks the budget after each node. `set_work_limit` starts a
fresh budget only between evaluations.

**Why:** a permission decision must have a deterministic CPU bound. The server
shares one budget across bind programs, reference retries and nested
comprehensions, and needs an error it can map to a stable
`CEL work limit exceeded` response instead of depending on wall-clock timeouts.

**Tests:** `server-rs/vendor/cel/src/lib.rs`
`work_limit_is_shared_by_nested_comprehensions_and_custom_functions`,
`work_limit_cannot_be_swallowed_by_a_custom_function`,
`work_limit_has_exact_boundaries_and_is_not_reset_by_execute`;
`server-rs/crates/instant-permissions/src/lib.rs`
`nested_comprehensions_exhaust_work_limit`,
`work_limit_is_shared_across_permission_binds`,
`work_limit_preserves_normal_rules_and_fresh_evaluations`,
`reference_retries_share_one_work_budget`; and the fuel-parity tests in
`server-rs/crates/instant-permissions/src/binding_conversion_tests.rs`.
