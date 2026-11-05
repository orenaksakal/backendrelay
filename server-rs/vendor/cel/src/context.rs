use crate::common::value::Val;
use crate::magic::{Function, FunctionRegistry, IntoFunction};
use crate::objects::{TryIntoValue, Value};
use crate::parser::Expression;
use crate::{Env, ExecutionError};
use std::borrow::Cow;
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, LazyLock};

// Standard overload declarations are immutable. Rebuilding them for each entity
// evaluation is unnecessary; variables, custom functions and fuel remain local
// to each Context. Caller-supplied environments still use with_env unchanged.
static STANDARD_ENV: LazyLock<Arc<Env>> = LazyLock::new(|| Arc::new(Env::stdlib()));
static STANDARD_VARIABLES: LazyLock<BTreeMap<String, Box<dyn Val>>> = LazyLock::new(|| {
    [
        ("int", &crate::common::types::INT_TYPE),
        ("uint", &crate::common::types::UINT_TYPE),
        ("bool", &crate::common::types::BOOL_TYPE),
        ("double", &crate::common::types::DOUBLE_TYPE),
        ("bytes", &crate::common::types::BYTES_TYPE),
        ("string", &crate::common::types::STRING_TYPE),
        ("dyn", &crate::common::types::DYN_TYPE),
        ("type", &crate::common::types::TYPE_TYPE),
        ("null_type", &crate::common::types::NULL_TYPE),
        ("list", &crate::common::types::LIST_TYPE),
        ("map", &crate::common::types::MAP_TYPE),
        ("timestamp", &crate::common::types::TIMESTAMP_TYPE),
        ("duration", &crate::common::types::DURATION_TYPE),
    ]
    .into_iter()
    .map(|(name, value)| (name.to_owned(), Box::new(value.to_owned()) as Box<dyn Val>))
    .collect()
});

/// Context is a collection of variables and functions that can be used
/// by the interpreter to resolve expressions.
///
/// The context can be either a parent context, or a child context. A
/// parent context is created by default and contains all of the built-in
/// functions. A child context can be created by calling `.new_inner_scope()`. The
/// child context has it's own variables (which can be added to), but it
/// will also reference the parent context. This allows for variables to
/// be overridden within the child context while still being able to
/// resolve variables in the child's parents. You can have theoretically
/// have an infinite number of child contexts that reference each-other.
///
/// So why is this important? Well some CEL-macros such as the `.map` macro
/// declare intermediate user-specified identifiers that should only be
/// available within the macro, and should not override variables in the
/// parent context. The `.map` macro can create a child context from the parent, add the
/// intermediate identifier to the child context, and then evaluate the
/// map expression.
///
/// Intermediate variable stored in child context
///               ↓
/// [1, 2, 3].map(x, x * 2) == [2, 4, 6]
///                  ↑
/// Only in scope for the duration of the map expression
///
pub enum Context<'a> {
    Root {
        functions: FunctionRegistry,
        function_parent: Option<&'a Context<'a>>,
        variables: BTreeMap<String, Box<dyn Val>>,
        standard_variables: bool,
        resolver: Option<&'a dyn VariableResolver>,
        env: Arc<Env>,
        work_remaining: AtomicU64,
        work_exhausted: AtomicBool,
    },
    Child {
        parent: &'a Context<'a>,
        variables: BTreeMap<String, Box<dyn Val>>,
        resolver: Option<&'a dyn VariableResolver>,
    },
}

impl<'a> Context<'a> {
    /// Starts a shared budget of expression visits, including comprehension loop
    /// conditions and bodies. Executing another program does not reset it.
    /// Call only between evaluations; child scopes share their root's budget.
    /// This does not bound parsing, allocation, or work inside native functions.
    pub fn set_work_limit(&self, limit: u64) {
        match self {
            Context::Root {
                work_remaining,
                work_exhausted,
                ..
            } => {
                work_remaining.store(limit, Ordering::Relaxed);
                work_exhausted.store(false, Ordering::Relaxed);
            }
            Context::Child { parent, .. } => parent.set_work_limit(limit),
        }
    }

    /// Remaining fuel, for callers resuming evaluation after external I/O.
    pub fn work_remaining(&self) -> u64 {
        match self {
            Self::Root { work_remaining, .. } => work_remaining.load(Ordering::Relaxed),
            Self::Child { parent, .. } => parent.work_remaining(),
        }
    }

    pub(crate) fn consume_work(&self) -> Result<(), ExecutionError> {
        self.consume_work_units(1)
    }

    /// Charges variable-cost native work against the same terminal budget.
    pub fn consume_work_units(&self, units: u64) -> Result<(), ExecutionError> {
        match self {
            Context::Root {
                work_remaining,
                work_exhausted,
                ..
            } => {
                if work_remaining
                    .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |n| {
                        n.checked_sub(units)
                    })
                    .is_err()
                {
                    work_exhausted.store(true, Ordering::Relaxed);
                    return Err(ExecutionError::WorkLimitExceeded);
                }
                Ok(())
            }
            Context::Child { parent, .. } => parent.consume_work_units(units),
        }
    }

    pub(crate) fn exhaust_work(&self) {
        match self {
            Self::Root { work_exhausted, .. } => work_exhausted.store(true, Ordering::Relaxed),
            Self::Child { parent, .. } => parent.exhaust_work(),
        }
    }

    pub(crate) fn check_work_limit(&self) -> Result<(), ExecutionError> {
        match self {
            Context::Root { work_exhausted, .. } => {
                if work_exhausted.load(Ordering::Relaxed) {
                    Err(ExecutionError::WorkLimitExceeded)
                } else {
                    Ok(())
                }
            }
            Context::Child { parent, .. } => parent.check_work_limit(),
        }
    }

    pub fn add_variable<S, V>(
        &mut self,
        name: S,
        value: V,
    ) -> Result<(), <V as TryIntoValue>::Error>
    where
        S: Into<String>,
        V: TryIntoValue,
    {
        match self {
            Context::Root { variables, .. } => {
                let value = value.try_into_value()?;
                let value: Box<dyn Val> = value.try_into().unwrap();
                variables.insert(name.into(), value);
            }
            Context::Child { variables, .. } => {
                let value = value.try_into_value()?;
                let value: Box<dyn Val> = value.try_into().unwrap();
                variables.insert(name.into(), value);
            }
        }
        Ok(())
    }

    pub fn add_variable_from_value<S, V>(&mut self, name: S, value: V)
    where
        S: Into<String>,
        V: Into<Value>,
    {
        match self {
            Context::Root { variables, .. } => {
                let value = value.into();
                let value: Box<dyn Val> = value.try_into().unwrap();
                variables.insert(name.into(), value);
            }
            Context::Child { variables, .. } => {
                let value = value.into();
                let value: Box<dyn Val> = value.try_into().unwrap();
                variables.insert(name.into(), value);
            }
        }
    }

    /// Binds a variable to a custom [`Val`] implementation directly, without
    /// going through the [`Value`] enum.
    ///
    /// [`add_variable`](Self::add_variable) and
    /// [`add_variable_from_value`](Self::add_variable_from_value) convert their
    /// input into a [`Value`], whose compound variants ([`Value::Map`],
    /// [`Value::List`], and `Value::Struct`) hold eagerly-materialized contents.
    /// For a value that should resolve its contents *on access* instead — e.g.
    /// a large or recursive backing object (a protobuf message, a database
    /// row) where member access maps to
    /// [`Indexer::get`](crate::common::traits::Indexer::get) and is computed
    /// lazily — implement [`Val`] and the relevant operator traits (such as
    /// [`Indexer`](crate::common::traits::Indexer),
    /// [`Iterable`](crate::common::traits::Iterable),
    /// [`Sizer`](crate::common::traits::Sizer)) for your type and bind it here.
    /// The built-in implementations in
    /// [`common::types`](crate::common::types) (e.g. `DefaultMap`, `Struct`)
    /// are the reference for what to implement.
    ///
    /// ```ignore
    /// // `my_value` implements `Val` + `Indexer`, resolving fields on access.
    /// let mut ctx = Context::default();
    /// ctx.add_variable_as_val("input", Box::new(my_value));
    /// let program = Program::compile("input.field")?;
    /// // `input.field` calls `Indexer::get` on `my_value` only when evaluated.
    /// let result = program.execute(&ctx)?;
    /// ```
    pub fn add_variable_as_val<S>(&mut self, name: S, value: Box<dyn Val>)
    where
        S: Into<String>,
    {
        match self {
            Context::Root { variables, .. } => {
                variables.insert(name.into(), value);
            }
            Context::Child { variables, .. } => {
                variables.insert(name.into(), value);
            }
        }
    }

    pub fn set_variable_resolver(&mut self, r: &'a dyn VariableResolver) {
        match self {
            Context::Root { resolver, .. } => {
                *resolver = Some(r);
            }
            Context::Child { resolver, .. } => {
                *resolver = Some(r);
            }
        }
    }

    pub fn get_variable<S>(&'a self, name: S) -> Option<Cow<'a, dyn Val>>
    where
        S: AsRef<str>,
    {
        let name = name.as_ref();
        match self {
            Context::Child {
                variables,
                parent,
                resolver,
            } => resolver
                .and_then(|r| {
                    r.resolve(name)
                        .map(|v| Cow::<dyn Val>::Owned(v.try_into().unwrap()))
                })
                .or_else(|| {
                    variables
                        .get(name)
                        .map(|b| Cow::<dyn Val>::Borrowed(b.as_ref()))
                        .or_else(|| parent.get_variable(name))
                }),
            Context::Root {
                variables,
                resolver,
                standard_variables,
                ..
            } => resolver
                .and_then(|r| {
                    r.resolve(name)
                        .map(|v| Cow::<dyn Val>::Owned(v.try_into().unwrap()))
                })
                .or_else(|| {
                    variables
                        .get(name)
                        .or_else(|| {
                            standard_variables
                                .then(|| STANDARD_VARIABLES.get(name))
                                .flatten()
                        })
                        .map(|v| Cow::<dyn Val>::Borrowed(v.as_ref()))
                }),
        }
    }

    pub(crate) fn env(&self) -> &Env {
        match self {
            Context::Root { env, .. } => env.as_ref(),
            Context::Child { parent, .. } => parent.env(),
        }
    }

    #[allow(dead_code)]
    pub(crate) fn get_function(&self, name: &str) -> Option<&Function> {
        match self {
            Context::Root {
                functions,
                function_parent,
                ..
            } => functions
                .get(name)
                .or_else(|| function_parent.and_then(|parent| parent.get_function(name))),
            Context::Child { parent, .. } => parent.get_function(name),
        }
    }

    pub fn add_function<T: 'static, F>(&mut self, name: &str, value: F)
    where
        F: IntoFunction<T> + 'static + Send + Sync,
    {
        if let Context::Root { functions, .. } = self {
            functions.add(name, value);
        };
    }

    pub fn resolve(&self, expr: &Expression) -> Result<Value, ExecutionError> {
        Value::resolve(expr, self)
    }

    pub fn resolve_all(&self, exprs: &[Expression]) -> Result<Value, ExecutionError> {
        Value::resolve_all(exprs, self)
    }

    pub fn new_inner_scope(&self) -> Context<'_> {
        Context::Child {
            parent: self,
            variables: Default::default(),
            resolver: None,
        }
    }

    /// Starts a fresh default root that shares only immutable native functions.
    /// Variables, resolvers and work budgets are not inherited. Unlike a child
    /// scope, this root can register local function overrides and owns its fuel.
    pub fn new_function_scope(&self) -> Context<'_> {
        let mut context = Context::default();
        if let Context::Root {
            function_parent, ..
        } = &mut context
        {
            *function_parent = Some(self);
        }
        context
    }

    /// Constructs a new empty context with no variables or functions.
    ///
    /// If you're looking for a context that has all the standard methods, functions
    /// and macros already added to the context, use [`Context::default`] instead.
    ///
    /// # Example
    /// ```
    /// use cel::Context;
    /// let mut context = Context::empty();
    /// context.add_function("add", |a: i64, b: i64| a + b);
    /// ```
    pub fn empty() -> Self {
        Context::Root {
            work_remaining: AtomicU64::new(u64::MAX),
            work_exhausted: AtomicBool::new(false),
            env: Arc::new(Env::default()),
            variables: Default::default(),
            standard_variables: false,
            functions: Default::default(),
            function_parent: None,
            resolver: None,
        }
    }

    pub fn with_env(env: Arc<Env>) -> Self {
        Context::Root {
            work_remaining: AtomicU64::new(u64::MAX),
            work_exhausted: AtomicBool::new(false),
            env,
            variables: Default::default(),
            standard_variables: false,
            functions: Default::default(),
            function_parent: None,
            resolver: None,
        }
    }
}

impl Default for Context<'_> {
    fn default() -> Self {
        Context::Root {
            env: Arc::clone(&STANDARD_ENV),
            work_remaining: AtomicU64::new(u64::MAX),
            work_exhausted: AtomicBool::new(false),
            variables: Default::default(),
            standard_variables: true,
            functions: Default::default(),
            function_parent: None,
            resolver: None,
        }
    }
}

/// VariableResolver implements a custom resolver for variables that is consulted before looking at
/// variables added to the context. This allows dynamic variables, or avoiding HashMap lookup/creation.
///
///
/// # Example
/// ```
/// struct ValueContext {
///     request: cel::Value,
///     response: cel::Value,
/// }
///
/// impl cel::context::VariableResolver for ValueContext {
///     fn resolve(&self, variable: &str) -> Option<cel::Value> {
///         match variable {
///             "request" => Some(self.request.clone()),
///             "response" => Some(self.response.clone()),
///             _ => None,
///         }
///     }
/// }
/// ```
pub trait VariableResolver: Send + Sync {
    fn resolve(&self, variable: &str) -> Option<Value>;
}

impl<T: VariableResolver> VariableResolver for Box<T> {
    fn resolve(&self, variable: &str) -> Option<Value> {
        (**self).resolve(variable)
    }
}

impl<T: VariableResolver> VariableResolver for Arc<T> {
    fn resolve(&self, variable: &str) -> Option<Value> {
        (**self).resolve(variable)
    }
}

impl<T: VariableResolver> VariableResolver for &T {
    fn resolve(&self, variable: &str) -> Option<Value> {
        (**self).resolve(variable)
    }
}

#[cfg(test)]
mod test {
    use super::Context;
    use crate::{ExecutionError, Program, Value};
    use std::sync::Arc;

    // A helper function that requires T to implement some traits
    fn assert_send<T: Send>() {}

    #[test]
    fn test_context_is_send() {
        // This line will only compile if assertion passes
        assert_send::<super::Context>();
    }

    #[test]
    fn default_contexts_share_standard_declarations_not_bindings_functions_or_fuel() {
        let mut first = Context::default();
        let mut second = Context::default();
        match (&first, &second) {
            (Context::Root { env: left, .. }, Context::Root { env: right, .. }) => {
                assert!(Arc::ptr_eq(left, right));
            }
            _ => unreachable!("default contexts are roots"),
        }
        first.add_variable("subject", 1_i64).unwrap();
        second.add_variable("subject", 2_i64).unwrap();
        let subject = Program::compile("subject").unwrap();
        assert_eq!(subject.execute(&first).unwrap(), Value::Int(1));
        assert_eq!(subject.execute(&second).unwrap(), Value::Int(2));

        first.add_function("only_first", || true);
        let custom = Program::compile("only_first()").unwrap();
        assert_eq!(custom.execute(&first).unwrap(), Value::Bool(true));
        assert!(custom.execute(&second).is_err());

        first.set_work_limit(0);
        let literal = Program::compile("true").unwrap();
        assert!(matches!(
            literal.execute(&first),
            Err(ExecutionError::WorkLimitExceeded)
        ));
        assert_eq!(literal.execute(&second).unwrap(), Value::Bool(true));
    }

    #[test]
    fn standard_type_bindings_are_immutable_fallbacks_with_local_override_priority() {
        let mut first = Context::default();
        let second = Context::default();
        let type_check = Program::compile("type(1) == int").unwrap();
        assert_eq!(type_check.execute(&first).unwrap(), Value::Bool(true));
        assert_eq!(type_check.execute(&second).unwrap(), Value::Bool(true));
        first.add_variable("int", 9_i64).unwrap();
        let identifier = Program::compile("int").unwrap();
        assert_eq!(identifier.execute(&first).unwrap(), Value::Int(9));
        assert_eq!(
            identifier.execute(&first.new_inner_scope()).unwrap(),
            Value::Int(9)
        );
        assert_eq!(type_check.execute(&second).unwrap(), Value::Bool(true));
        assert!(identifier.execute(&Context::empty()).is_err());
        let custom = Context::with_env(Arc::new(crate::Env::stdlib()));
        assert!(identifier.execute(&custom).is_err());
    }

    #[test]
    fn function_scopes_share_definitions_not_variables_overrides_resolvers_or_fuel() {
        struct Resolver;
        impl super::VariableResolver for Resolver {
            fn resolve(&self, name: &str) -> Option<Value> {
                (name == "parent_resolved").then_some(Value::Int(7))
            }
        }
        let resolver = Resolver;
        let mut definitions = Context::empty();
        definitions.set_variable_resolver(&resolver);
        definitions.add_variable("private_subject", 7_i64).unwrap();
        definitions.add_function("identity", |value: i64| value);
        definitions.set_work_limit(0);
        let mut first = definitions.new_function_scope();
        let second = definitions.new_function_scope();
        first.add_function("identity", |value: i64| value + 1);
        let function = Program::compile("identity(1)").unwrap();
        assert_eq!(function.execute(&first).unwrap(), Value::Int(2));
        assert_eq!(function.execute(&second).unwrap(), Value::Int(1));
        for name in ["private_subject", "parent_resolved"] {
            assert!(Program::compile(name).unwrap().execute(&first).is_err());
        }
        first.set_work_limit(0);
        assert!(matches!(
            function.execute(&first),
            Err(ExecutionError::WorkLimitExceeded)
        ));
        assert_eq!(function.execute(&second).unwrap(), Value::Int(1));
        std::thread::scope(|scope| {
            for _ in 0..8 {
                let definitions = &definitions;
                let function = &function;
                scope.spawn(move || {
                    let context = definitions.new_function_scope();
                    assert_eq!(function.execute(&context).unwrap(), Value::Int(1));
                });
            }
        });
    }
}
