use std::collections::HashMap;
use std::fmt::Debug;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::Value as JsonValue;
use uuid::Uuid;

use crate::{PermissionAction, PermissionError, RuleSet, normalize_binds};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RuleValidationIssue {
    pub message: String,
    pub path: Vec<String>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RefillType {
    Greedy,
    Interval,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RateLimit {
    pub capacity: u64,
    pub refill_amount: u64,
    pub refill_period: Duration,
    pub refill_type: RefillType,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RateLimitBucketConfig {
    pub limits: Vec<RateLimit>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct RateLimitRequest {
    pub app_id: Uuid,
    pub bucket_name: String,
    pub config: RateLimitBucketConfig,
    pub bucket_key: JsonValue,
    pub tokens: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RateLimitDecision {
    Allowed {
        remaining_tokens: u64,
    },
    Denied {
        retry_after: Duration,
        remaining_tokens: u64,
    },
}

/// Synchronous consumption seam used by CEL evaluation. Implementations must
/// atomically apply every bandwidth in a bucket configuration.
pub trait RateLimitConsumer: Debug + Send + Sync {
    /// Attempts to consume the requested tokens.
    ///
    /// # Errors
    ///
    /// Returns a backend-specific error message when the bucket cannot be read
    /// or updated. Exhaustion is returned as [`RateLimitDecision::Denied`].
    fn consume(&self, request: &RateLimitRequest) -> Result<RateLimitDecision, String>;

    /// Attempts to consume several independent buckets as one logical action.
    /// Implementations with transactional storage should override this so no
    /// bucket is charged when any member is exhausted.
    ///
    /// # Errors
    ///
    /// Returns a backend error or one decision per request in input order.
    fn consume_all(&self, requests: &[RateLimitRequest]) -> Result<Vec<RateLimitDecision>, String> {
        requests
            .iter()
            .map(|request| self.consume(request))
            .collect()
    }

    /// Clears a bucket so its next consumption starts from full capacity.
    /// Buckets that were never created are already empty, so this is a no-op
    /// for them.
    ///
    /// # Errors
    ///
    /// Returns a backend-specific error message when the bucket cannot be
    /// cleared.
    fn reset(&self, _request: &RateLimitRequest) -> Result<(), String> {
        Ok(())
    }
}

#[doc(hidden)]
#[derive(Clone, Debug)]
pub struct RateLimitRuntime {
    pub app_id: Uuid,
    pub configs: Arc<HashMap<String, RateLimitBucketConfig>>,
    pub consumer: Arc<dyn RateLimitConsumer>,
}

#[derive(Debug, Default)]
pub struct InMemoryRateLimiter {
    buckets: Mutex<HashMap<String, BucketState>>,
}

#[derive(Debug)]
struct BucketState {
    limits: Vec<LimitState>,
}

#[derive(Debug)]
struct LimitState {
    tokens: u128,
    last_refill: Instant,
}

const TOKEN_SCALE: u128 = 1_000_000_000;

impl RateLimitConsumer for InMemoryRateLimiter {
    fn consume(&self, request: &RateLimitRequest) -> Result<RateLimitDecision, String> {
        if request.tokens == 0 {
            return Err("rate limit tokens must be a positive integer".to_owned());
        }
        let key = bucket_storage_key(request)?;
        let now = Instant::now();
        let mut buckets = self
            .buckets
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let bucket = buckets.entry(key).or_insert_with(|| BucketState {
            limits: request
                .config
                .limits
                .iter()
                .map(|limit| LimitState {
                    tokens: scaled_tokens(limit.capacity),
                    last_refill: now,
                })
                .collect(),
        });
        if bucket.limits.len() != request.config.limits.len() {
            return Err("stored rate limit bucket has incompatible bandwidths".to_owned());
        }
        for (state, limit) in bucket.limits.iter_mut().zip(&request.config.limits) {
            refill(state, limit, now);
        }
        let requested = scaled_tokens(request.tokens);
        if bucket.limits.iter().all(|limit| limit.tokens >= requested) {
            for limit in &mut bucket.limits {
                limit.tokens -= requested;
            }
            let remaining_tokens = bucket
                .limits
                .iter()
                .map(|limit| whole_tokens(limit.tokens))
                .min()
                .unwrap_or(0);
            return Ok(RateLimitDecision::Allowed { remaining_tokens });
        }
        let retry_after = bucket
            .limits
            .iter()
            .zip(&request.config.limits)
            .filter(|(state, _)| state.tokens < requested)
            .map(|(state, limit)| retry_after(state, limit, requested, now))
            .max()
            .unwrap_or(Duration::ZERO);
        let remaining_tokens = bucket
            .limits
            .iter()
            .map(|limit| whole_tokens(limit.tokens))
            .min()
            .unwrap_or(0);
        Ok(RateLimitDecision::Denied {
            retry_after,
            remaining_tokens,
        })
    }

    fn reset(&self, request: &RateLimitRequest) -> Result<(), String> {
        let key = bucket_storage_key(request)?;
        self.buckets
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&key);
        Ok(())
    }
}

fn refill(state: &mut LimitState, limit: &RateLimit, now: Instant) {
    let elapsed = now.saturating_duration_since(state.last_refill);
    match limit.refill_type {
        RefillType::Greedy => {
            let added = elapsed
                .as_nanos()
                .saturating_mul(u128::from(limit.refill_amount))
                .saturating_mul(TOKEN_SCALE)
                / limit.refill_period.as_nanos();
            state.tokens = state
                .tokens
                .saturating_add(added)
                .min(scaled_tokens(limit.capacity));
            state.last_refill = now;
        }
        RefillType::Interval => {
            let period_nanos = limit.refill_period.as_nanos();
            let intervals = elapsed.as_nanos() / period_nanos;
            if intervals > 0 {
                let added = intervals
                    .saturating_mul(u128::from(limit.refill_amount))
                    .saturating_mul(TOKEN_SCALE);
                state.tokens = state
                    .tokens
                    .saturating_add(added)
                    .min(scaled_tokens(limit.capacity));
                let advanced_nanos = period_nanos.saturating_mul(intervals);
                let advanced =
                    Duration::from_nanos(u64::try_from(advanced_nanos).unwrap_or(u64::MAX));
                state.last_refill += advanced;
            }
        }
    }
}

fn retry_after(state: &LimitState, limit: &RateLimit, requested: u128, now: Instant) -> Duration {
    match limit.refill_type {
        RefillType::Greedy => {
            let missing = requested.saturating_sub(state.tokens);
            let denominator = u128::from(limit.refill_amount).saturating_mul(TOKEN_SCALE);
            duration_from_nanos(ceil_div(
                missing.saturating_mul(limit.refill_period.as_nanos()),
                denominator,
            ))
        }
        RefillType::Interval => {
            let missing = requested.saturating_sub(state.tokens);
            let intervals = ceil_div(missing, scaled_tokens(limit.refill_amount));
            let elapsed = now.saturating_duration_since(state.last_refill);
            duration_from_nanos(limit.refill_period.as_nanos().saturating_mul(intervals))
                .saturating_sub(elapsed)
        }
    }
}

fn scaled_tokens(tokens: u64) -> u128 {
    u128::from(tokens).saturating_mul(TOKEN_SCALE)
}

fn whole_tokens(tokens: u128) -> u64 {
    u64::try_from(tokens / TOKEN_SCALE).unwrap_or(u64::MAX)
}

const fn ceil_div(numerator: u128, denominator: u128) -> u128 {
    numerator.saturating_add(denominator.saturating_sub(1)) / denominator
}

fn duration_from_nanos(nanos: u128) -> Duration {
    Duration::from_nanos(u64::try_from(nanos).unwrap_or(u64::MAX))
}

fn bucket_storage_key(request: &RateLimitRequest) -> Result<String, String> {
    let config = serde_json::to_string(&rate_limit_config_json(&request.config))
        .map_err(|error| error.to_string())?;
    let bucket_key =
        serde_json::to_string(&request.bucket_key).map_err(|error| error.to_string())?;
    Ok(format!(
        "{}:{}:{config}:{bucket_key}",
        request.app_id, request.bucket_name
    ))
}

fn rate_limit_config_json(config: &RateLimitBucketConfig) -> JsonValue {
    JsonValue::Array(
        config
            .limits
            .iter()
            .map(|limit| {
                serde_json::json!({
                    "capacity": limit.capacity,
                    "refillAmount": limit.refill_amount,
                    "refillPeriodNanos": limit.refill_period.as_nanos().to_string(),
                    "refillType": match limit.refill_type {
                        RefillType::Greedy => "greedy",
                        RefillType::Interval => "interval",
                    }
                })
            })
            .collect(),
    )
}

impl RateLimitBucketConfig {
    /// Parses one `$rateLimits` bucket using the JVM defaults and bounds.
    ///
    /// # Errors
    ///
    /// Returns an invalid-rules error for missing/empty limits, non-positive
    /// integer capacities/refills, invalid refill modes, or periods outside the
    /// supported one-second through less-than-25-hours window.
    pub fn parse(value: &JsonValue) -> Result<Self, PermissionError> {
        let object = value.as_object().ok_or_else(|| {
            PermissionError::InvalidRules("rate limit config must be an object".to_owned())
        })?;
        let limits = object
            .get("limits")
            .and_then(JsonValue::as_array)
            .filter(|limits| !limits.is_empty())
            .ok_or_else(|| {
                PermissionError::InvalidRules(
                    "the rate limit config must have at least one limit in the `limits` array"
                        .to_owned(),
                )
            })?;
        Ok(Self {
            limits: limits
                .iter()
                .map(parse_limit)
                .collect::<Result<Vec<_>, _>>()?,
        })
    }
}

impl RuleSet {
    /// Constructs and fully validates a persisted permission rule document.
    ///
    /// # Errors
    ///
    /// Returns all independently discoverable document, bind, CEL, system
    /// namespace, field, and rate-limit validation issues.
    pub fn try_new(code: JsonValue) -> Result<Self, Vec<RuleValidationIssue>> {
        let rules = Self::new(code);
        let errors = rules.validation_errors();
        if errors.is_empty() {
            Ok(rules)
        } else {
            Err(errors)
        }
    }

    /// Validates the complete rule document using the same four object actions
    /// and special namespace constraints as the JVM model layer.
    #[must_use]
    pub fn validation_errors(&self) -> Vec<RuleValidationIssue> {
        let Some(rules) = self.code().as_object() else {
            return vec![issue(std::iter::empty::<&str>(), "rules must be an object")];
        };
        let mut errors = validate_binds(rules);
        let mut etypes = rules
            .keys()
            .filter(|etype| etype.as_str() != "$rateLimits")
            .cloned()
            .collect::<Vec<_>>();
        etypes.sort();
        for etype in etypes {
            for (action_name, action) in object_actions() {
                if is_restricted_system_namespace(&etype) {
                    errors.push(issue(
                        [&etype, "allow", action_name],
                        format!(
                            "The {etype} namespace is a reserved internal namespace that does not yet support rules."
                        ),
                    ));
                    continue;
                }
                let expression = rules
                    .get(&etype)
                    .and_then(JsonValue::as_object)
                    .and_then(|rule| rule.get("allow"))
                    .and_then(JsonValue::as_object)
                    .and_then(|allow| allow.get(action_name))
                    .filter(|expression| !expression.is_null());
                if etype == "$users"
                    && action == PermissionAction::Delete
                    && expression.is_some_and(|expression| !is_false_rule(expression))
                {
                    errors.push(issue(
                        [&etype, "allow", action_name],
                        "The $users namespace doesn't support permissions for delete. Set `$users.allow.delete` to `\"false\"`."
                    ));
                    continue;
                }
                let Some(expression) = expression else {
                    continue;
                };
                validate_expression_at(self, &etype, action, expression, &mut errors);
            }
            validate_fields(self, &etype, &mut errors);
        }
        validate_rate_limit_configs(self, &mut errors);
        errors
    }

    /// Parses all configured `$rateLimits` buckets after validation.
    ///
    /// # Errors
    ///
    /// Returns an error when `$rateLimits` is not an object or any bucket is
    /// invalid.
    pub fn rate_limit_configs(
        &self,
    ) -> Result<std::collections::HashMap<String, RateLimitBucketConfig>, PermissionError> {
        let Some(value) = self.code().get("$rateLimits") else {
            return Ok(std::collections::HashMap::new());
        };
        if value.is_null() {
            return Ok(std::collections::HashMap::new());
        }
        let values = value.as_object().ok_or_else(|| {
            PermissionError::InvalidRules("$rateLimits must be an object".to_owned())
        })?;
        values
            .iter()
            .map(|(name, value)| Ok((name.clone(), RateLimitBucketConfig::parse(value)?)))
            .collect()
    }

    /// Adds the configured CEL `rateLimit` buckets and their consumer to an
    /// evaluation binding set.
    ///
    /// # Errors
    ///
    /// Returns an error when any persisted bucket configuration is invalid.
    pub fn configure_rate_limits(
        &self,
        app_id: Uuid,
        bindings: &mut crate::EvaluationBindings,
        consumer: Arc<dyn RateLimitConsumer>,
    ) -> Result<(), PermissionError> {
        let configs = Arc::new(self.rate_limit_configs()?);
        bindings.rate_limit = JsonValue::Object(
            configs
                .keys()
                .map(|name| {
                    (
                        name.clone(),
                        serde_json::json!({"__instant_rate_limit_bucket": name}),
                    )
                })
                .collect(),
        );
        bindings.rate_limit_runtime = Some(RateLimitRuntime {
            app_id,
            configs,
            consumer,
        });
        Ok(())
    }
}

fn object_actions() -> [(&'static str, PermissionAction); 4] {
    [
        ("view", PermissionAction::View),
        ("create", PermissionAction::Create),
        ("update", PermissionAction::Update),
        ("delete", PermissionAction::Delete),
    ]
}

fn validate_binds(rules: &serde_json::Map<String, JsonValue>) -> Vec<RuleValidationIssue> {
    let mut errors = Vec::new();
    let mut etypes = rules.keys().cloned().collect::<Vec<_>>();
    etypes.sort();
    for etype in etypes {
        let Some(bind) = rules
            .get(&etype)
            .and_then(JsonValue::as_object)
            .and_then(|rule| rule.get("bind"))
            .filter(|bind| !bind.is_null())
        else {
            continue;
        };
        let binds = match normalize_binds(Some(bind)) {
            Ok(binds) => binds,
            Err(error) => {
                errors.push(issue([&etype, "bind"], error.to_string()));
                continue;
            }
        };
        let mut seen = std::collections::HashSet::new();
        if let Some((duplicate, _)) = binds.iter().find(|(name, _)| !seen.insert(name.clone())) {
            errors.push(issue(
                [&etype, "bind", duplicate],
                "bind should only contain a given variable name once",
            ));
        }
    }
    errors
}

fn validate_expression_at(
    rules: &RuleSet,
    etype: &str,
    action: PermissionAction,
    expression: &JsonValue,
    errors: &mut Vec<RuleValidationIssue>,
) {
    let path = vec![
        etype.to_owned(),
        "allow".to_owned(),
        action.as_str().to_owned(),
    ];
    match rules.compile_selected_without_rate_limit_validation(etype, action, expression) {
        Ok(program) => {
            let configured = rules
                .code()
                .get("$rateLimits")
                .and_then(JsonValue::as_object);
            for name in program.rate_limit_uses() {
                if configured.is_none_or(|configured| !configured.contains_key(name)) {
                    errors.push(RuleValidationIssue {
                        message: format!(
                            "`{name}` is not a valid rate limit config. It should be defined in the `$rateLimits` key."
                        ),
                        path: path.clone(),
                    });
                }
            }
        }
        Err(error) => errors.push(RuleValidationIssue {
            message: error.to_string(),
            path,
        }),
    }
}

fn validate_fields(rules: &RuleSet, etype: &str, errors: &mut Vec<RuleValidationIssue>) {
    let Some(fields) = rules
        .code()
        .get(etype)
        .and_then(JsonValue::as_object)
        .and_then(|rule| rule.get("fields"))
        .and_then(JsonValue::as_object)
    else {
        return;
    };
    let mut names = fields.keys().cloned().collect::<Vec<_>>();
    names.sort();
    for field in names {
        let expression = &fields[&field];
        if expression.is_null() {
            continue;
        }
        if field == "id" {
            errors.push(issue(
                [etype, "fields"],
                format!(
                    "You cannot set field rules for `id`. Use {etype} -> allow -> view instead"
                ),
            ));
        } else {
            let before = errors.len();
            validate_expression_at(rules, etype, PermissionAction::View, expression, errors);
            for error in &mut errors[before..] {
                error.path = vec![etype.to_owned(), "fields".to_owned(), field.clone()];
            }
        }
    }
}

fn validate_rate_limit_configs(rules: &RuleSet, errors: &mut Vec<RuleValidationIssue>) {
    let Some(value) = rules.code().get("$rateLimits") else {
        return;
    };
    if value.is_null() {
        return;
    }
    let Some(configs) = value.as_object() else {
        errors.push(issue(["$rateLimits"], "$rateLimits must be an object"));
        return;
    };
    let mut names = configs.keys().cloned().collect::<Vec<_>>();
    names.sort();
    for name in names {
        if let Err(error) = RateLimitBucketConfig::parse(&configs[&name]) {
            errors.push(issue(["$rateLimits", &name], error.to_string()));
        }
    }
}

fn is_restricted_system_namespace(etype: &str) -> bool {
    etype.starts_with('$')
        && !matches!(
            etype,
            "$users" | "$files" | "$default" | "$streams" | "$rateLimits"
        )
}

fn is_false_rule(value: &JsonValue) -> bool {
    value == "false" || value == &JsonValue::Bool(false)
}

fn issue<I, S>(path: I, message: impl Into<String>) -> RuleValidationIssue
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    RuleValidationIssue {
        message: message.into(),
        path: path
            .into_iter()
            .map(|segment| segment.as_ref().to_owned())
            .collect(),
    }
}

fn parse_limit(value: &JsonValue) -> Result<RateLimit, PermissionError> {
    let object = value
        .as_object()
        .ok_or_else(|| PermissionError::InvalidRules("rate limit must be an object".to_owned()))?;
    let capacity = positive_integer(object.get("capacity"), "capacity")?;
    let refill =
        match object.get("refill") {
            None => None,
            Some(value) => Some(value.as_object().ok_or_else(|| {
                PermissionError::InvalidRules("refill must be an object".to_owned())
            })?),
        };
    let refill_amount = refill
        .and_then(|refill| refill.get("amount"))
        .map(|value| positive_integer(Some(value), "refill.amount"))
        .transpose()?
        .unwrap_or(capacity);
    let refill_type = match refill
        .and_then(|refill| refill.get("type"))
        .and_then(JsonValue::as_str)
        .unwrap_or("greedy")
    {
        "greedy" => RefillType::Greedy,
        "interval" => RefillType::Interval,
        _ => {
            return Err(PermissionError::InvalidRules(
                "refill.type must be `greedy` or `interval`".to_owned(),
            ));
        }
    };
    let period = refill
        .and_then(|refill| refill.get("period"))
        .and_then(JsonValue::as_str)
        .unwrap_or("1 hour");
    let refill_period = parse_duration(period)?;
    if refill_period.as_secs() == 0 {
        return Err(PermissionError::InvalidRules(
            "the refill period must be longer than a second".to_owned(),
        ));
    }
    if refill_period.as_secs() / 3_600 > 24 {
        return Err(PermissionError::InvalidRules(
            "the refill period can't be longer than a day".to_owned(),
        ));
    }
    Ok(RateLimit {
        capacity,
        refill_amount,
        refill_period,
        refill_type,
    })
}

fn positive_integer(value: Option<&JsonValue>, path: &str) -> Result<u64, PermissionError> {
    value
        .and_then(JsonValue::as_u64)
        .filter(|value| *value > 0)
        .ok_or_else(|| PermissionError::InvalidRules(format!("{path} must be a positive integer")))
}

/// Parses the PostgreSQL-style duration units accepted by JVM rule configs.
/// Years and months use the JVM compatibility values of 365 and 30 days.
///
/// # Errors
///
/// Returns an invalid-rules error for malformed, negative, fractional, or
/// unsupported duration terms and for arithmetic overflow.
pub fn parse_duration(value: &str) -> Result<Duration, PermissionError> {
    let mut components = IntervalComponents::default();
    let trimmed = value.trim();
    if trimmed.starts_with('P') {
        parse_iso_interval(trimmed, &mut components).map_err(|()| invalid_duration(value))?;
    } else {
        parse_postgres_interval(trimmed, &mut components).map_err(|()| invalid_duration(value))?;
    }
    components.duration().ok_or_else(|| invalid_duration(value))
}

#[derive(Default)]
struct IntervalComponents {
    years: i32,
    months: i32,
    days: i32,
    hours: i32,
    minutes: i32,
    seconds: i64,
}

impl IntervalComponents {
    fn duration(&self) -> Option<Duration> {
        let total = i128::from(self.years)
            .checked_mul(365 * 86_400)?
            .checked_add(i128::from(self.months).checked_mul(30 * 86_400)?)?
            .checked_add(i128::from(self.days).checked_mul(86_400)?)?
            .checked_add(i128::from(self.hours).checked_mul(3_600)?)?
            .checked_add(i128::from(self.minutes).checked_mul(60)?)?
            .checked_add(i128::from(self.seconds))?;
        u64::try_from(total).ok().map(Duration::from_secs)
    }

    fn negate(&mut self) {
        self.years = self.years.saturating_neg();
        self.months = self.months.saturating_neg();
        self.days = self.days.saturating_neg();
        self.hours = self.hours.saturating_neg();
        self.minutes = self.minutes.saturating_neg();
        self.seconds = self.seconds.saturating_neg();
    }
}

fn parse_postgres_interval(value: &str, components: &mut IntervalComponents) -> Result<(), ()> {
    let normalized = postgres_interval_separators(value);
    let mut parts = normalized.split_whitespace().collect::<Vec<_>>();
    if parts.first() == Some(&"@") {
        parts.remove(0);
    }
    let ago = parts.last() == Some(&"ago");
    if ago {
        parts.pop();
    }
    let mut index = 0;
    while index < parts.len() {
        if parts[index].contains(':') {
            parse_colon_time(parts[index], components)?;
            index += 1;
            continue;
        }
        let Some(unit) = parts.get(index + 1).copied() else {
            break;
        };
        if set_interval_component(parts[index], unit, components)? {
            index += 2;
        } else {
            index += 1;
        }
    }
    if ago {
        components.negate();
    }
    Ok(())
}

fn postgres_interval_separators(value: &str) -> String {
    let mut normalized = String::with_capacity(value.len());
    let mut previous = None;
    for character in value.chars() {
        if character == '+' && !matches!(previous, Some('e' | 'E')) {
            normalized.push(' ');
        } else {
            normalized.push(character);
        }
        previous = Some(character);
    }
    normalized
}

fn set_interval_component(
    amount: &str,
    unit: &str,
    components: &mut IntervalComponents,
) -> Result<bool, ()> {
    let unit = unit.to_ascii_lowercase();
    if unit.starts_with("year") {
        components.years = amount.parse().map_err(|_| ())?;
    } else if unit.starts_with("mon") {
        components.months = amount.parse().map_err(|_| ())?;
    } else if unit.starts_with("day") {
        components.days = amount.parse().map_err(|_| ())?;
    } else if unit.starts_with("hour") {
        components.hours = amount.parse().map_err(|_| ())?;
    } else if unit.starts_with("min") {
        components.minutes = amount.parse().map_err(|_| ())?;
    } else if unit.starts_with("sec") {
        components.seconds = parse_interval_seconds(amount)?;
    } else {
        return Ok(false);
    }
    Ok(true)
}

#[allow(clippy::cast_possible_truncation)] // Mirrors PGInterval's double-to-Java-long boundary.
fn parse_interval_seconds(value: &str) -> Result<i64, ()> {
    let seconds = value.parse::<f64>().map_err(|_| ())?;
    if seconds.is_finite() {
        Ok(seconds as i64)
    } else {
        Err(())
    }
}

fn parse_colon_time(value: &str, components: &mut IntervalComponents) -> Result<(), ()> {
    let (sign, value) = value
        .strip_prefix('-')
        .map_or((1, value), |value| (-1, value));
    let value = value.strip_prefix('+').unwrap_or(value);
    let fields = value.split(':').collect::<Vec<_>>();
    if !(2..=3).contains(&fields.len()) {
        return Err(());
    }
    components.hours = sign * fields[0].parse::<i32>().map_err(|_| ())?;
    components.minutes = sign * fields[1].parse::<i32>().map_err(|_| ())?;
    components.seconds = if let Some(seconds) = fields.get(2) {
        i64::from(sign) * parse_interval_seconds(seconds)?
    } else {
        0
    };
    Ok(())
}

fn parse_iso_interval(value: &str, components: &mut IntervalComponents) -> Result<(), ()> {
    let mut in_time = false;
    let mut number_start = 1;
    for (index, character) in value.char_indices().skip(1) {
        if character == 'T' {
            if number_start != index {
                return Err(());
            }
            in_time = true;
            number_start = index + 1;
            continue;
        }
        let recognized = matches!(character, 'Y' | 'M' | 'D' | 'H' | 'S');
        if !recognized {
            continue;
        }
        let amount = value.get(number_start..index).ok_or(())?;
        if amount.is_empty() {
            return Err(());
        }
        match (in_time, character) {
            (false, 'Y') => components.years = amount.parse().map_err(|_| ())?,
            (false, 'M') => components.months = amount.parse().map_err(|_| ())?,
            (false, 'D') => components.days = amount.parse().map_err(|_| ())?,
            (true, 'H') => components.hours = amount.parse().map_err(|_| ())?,
            (true, 'M') => components.minutes = amount.parse().map_err(|_| ())?,
            (true, 'S') => components.seconds = parse_interval_seconds(amount)?,
            _ => return Err(()),
        }
        number_start = index + character.len_utf8();
    }
    if number_start == value.len() {
        Ok(())
    } else {
        Err(())
    }
}

fn invalid_duration(value: &str) -> PermissionError {
    PermissionError::InvalidRules(format!("invalid refill period `{value}`"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{EvaluationBindings, EvaluationItem, PermissionAction, RefCache, evaluate_batch};
    use serde_json::json;

    #[test]
    #[allow(clippy::duration_suboptimal_units)] // Exact seconds make JVM PGInterval parity explicit.
    fn duration_units_match_postgres_interval_compatibility_values() {
        assert_eq!(
            parse_duration("1 day").unwrap(),
            Duration::from_secs(86_400)
        );
        assert_eq!(
            parse_duration("1 day 2 hours").unwrap(),
            Duration::from_secs(93_600)
        );
        assert_eq!(
            parse_duration("1 year").unwrap(),
            Duration::from_secs(365 * 86_400)
        );
        assert_eq!(
            parse_duration("2 months").unwrap(),
            Duration::from_secs(60 * 86_400)
        );
    }

    #[test]
    fn duration_parser_matches_jdbc_pginterval_forms() {
        let hour = Duration::from_secs(3_600);
        for value in [
            "60 mins",
            "3600 secs",
            "@ 1 hour",
            "01:00:00",
            "PT1H",
            "-1 day 25 hours",
            "1+hour",
            "1 hour junk ignored",
        ] {
            assert_eq!(parse_duration(value).unwrap(), hour, "{value}");
        }
        assert_eq!(
            parse_duration("1.9 seconds").unwrap(),
            Duration::from_secs(1)
        );
        assert_eq!(
            parse_duration("1e1 seconds").unwrap(),
            Duration::from_secs(10)
        );
        assert_eq!(
            parse_duration("1 mon -29 days").unwrap(),
            Duration::from_hours(24)
        );
        assert_eq!(
            parse_duration("@ -1 day ago").unwrap(),
            Duration::from_hours(24)
        );
        assert_eq!(
            parse_duration("1 hour 2 hours").unwrap(),
            Duration::from_secs(7_200)
        );
        assert_eq!(parse_duration("1 week").unwrap(), Duration::ZERO);
        assert!(parse_duration("-1 hour").is_err());
    }

    #[test]
    fn bucket_defaults_and_multiple_limits_match_the_jvm_contract() {
        let config = RateLimitBucketConfig::parse(&json!({
            "limits": [
                {"capacity": 10},
                {"capacity": 5, "refill": {
                    "amount": 3, "period": "30 minutes", "type": "interval"
                }}
            ]
        }))
        .unwrap();
        assert_eq!(
            config.limits[0],
            RateLimit {
                capacity: 10,
                refill_amount: 10,
                refill_period: Duration::from_secs(3_600),
                refill_type: RefillType::Greedy,
            }
        );
        assert_eq!(config.limits[1].refill_amount, 3);
        assert_eq!(config.limits[1].refill_type, RefillType::Interval);
    }

    #[test]
    fn invalid_bucket_shapes_and_bounds_are_rejected() {
        for value in [
            json!({}),
            json!({"limits": []}),
            json!({"limits": [{"capacity": 0}]}),
            json!({"limits": [{"capacity": -1}]}),
            json!({"limits": [{"capacity": "abc"}]}),
            json!({"limits": [{"capacity": 1, "refill": {"type": "bad"}}]}),
            json!({"limits": [{"capacity": 1, "refill": {"period": "0 seconds"}}]}),
            json!({"limits": [{"capacity": 1, "refill": {"period": "2 days"}}]}),
        ] {
            assert!(RateLimitBucketConfig::parse(&value).is_err(), "{value}");
        }
    }

    #[test]
    fn rule_documents_validate_dot_and_index_rate_limit_references() {
        let valid = RuleSet::new(json!({
            "$rateLimits": {"myLimit": {"limits": [{"capacity": 2}]}},
            "things": {"allow": {"create":
                "rateLimit.myLimit.limit(auth.id) && rateLimit['myLimit'].limit(auth.id, 2)"
            }}
        }));
        assert!(valid.validation_errors().is_empty());
        assert_eq!(valid.rate_limit_configs().unwrap().len(), 1);

        let invalid = RuleSet::new(json!({
            "$rateLimits": {"someOtherLimit": {"limits": [{"capacity": 2}]}},
            "things": {"allow": {"view":
                "rateLimit.myLimit.limit('a') && rateLimit['myLimit2'].limit('b')"
            }}
        }));
        let errors = invalid.validation_errors();
        assert_eq!(errors.len(), 2);
        assert_eq!(errors[0].path, ["things", "allow", "view"]);
        assert!(
            errors
                .iter()
                .any(|error| error.message.contains("`myLimit`"))
        );
        assert!(
            errors
                .iter()
                .any(|error| error.message.contains("`myLimit2`"))
        );
    }

    #[test]
    fn document_validation_covers_system_fields_binds_and_config_shape() {
        let errors = RuleSet::new(json!({"$rateLimits": "bad"})).validation_errors();
        assert_eq!(
            errors,
            [issue(["$rateLimits"], "$rateLimits must be an object")]
        );

        let errors = RuleSet::new(json!({
            "$codes": {"allow": {"update": "true"}}
        }))
        .validation_errors();
        assert_eq!(errors.len(), 4);

        assert!(
            RuleSet::new(json!({"$users": {"allow": {"create": "true"}}}))
                .validation_errors()
                .is_empty()
        );
        assert_eq!(
            RuleSet::new(json!({"$users": {"allow": {"delete": "true"}}}))
                .validation_errors()
                .len(),
            1
        );

        let errors = RuleSet::new(json!({
            "things": {
                "bind": ["duplicate", "true", "duplicate", "false"],
                "fields": {"id": "true"},
                "allow": {"view": "duplicate"}
            }
        }))
        .validation_errors();
        assert!(
            errors
                .iter()
                .any(|error| error.path == ["things", "bind", "duplicate"])
        );
        assert!(
            errors
                .iter()
                .any(|error| error.path == ["things", "fields"])
        );
    }

    #[test]
    fn document_validation_rejects_incompatible_equality_types() {
        let errors = RuleSet::new(json!({
            "things": {"allow": {"view": "request.time != 0"}}
        }))
        .validation_errors();

        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0].path, ["things", "allow", "view"]);
        assert_eq!(
            errors[0].message,
            "invalid CEL types: found no matching overload for '_!=_'"
        );
    }

    #[test]
    fn document_validation_rejects_missing_arithmetic_overloads() {
        let errors = RuleSet::new(json!({
            "things": {"allow": {"view": "true ? true : 1 + false"}}
        }))
        .validation_errors();

        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0].path, ["things", "allow", "view"]);
        assert_eq!(
            errors[0].message,
            "invalid CEL types: found no matching overload for '_+_'"
        );
    }

    #[test]
    fn cel_rate_limit_calls_consume_by_app_bucket_config_and_key() {
        let rules = RuleSet::try_new(json!({
            "$rateLimits": {"createThings": {"limits": [{"capacity": 2}]}},
            "things": {"allow": {"create": "rateLimit.createThings.limit(auth.id)"}}
        }))
        .unwrap();
        let limiter: Arc<dyn RateLimitConsumer> = Arc::new(InMemoryRateLimiter::default());
        let app_id = Uuid::new_v4();
        let mut bindings = EvaluationBindings {
            auth: json!({"id": "user-1"}),
            ..EvaluationBindings::default()
        };
        rules
            .configure_rate_limits(app_id, &mut bindings, Arc::clone(&limiter))
            .unwrap();
        let program = rules
            .program("things", PermissionAction::Create)
            .unwrap()
            .unwrap();
        assert!(program.evaluate_bool(&bindings).unwrap());
        assert!(program.evaluate_bool(&bindings).unwrap());
        assert!(matches!(
            program.evaluate_bool(&bindings),
            Err(PermissionError::RateLimited {
                remaining_tokens: 0,
                ..
            })
        ));

        bindings.auth = json!({"id": "user-2"});
        assert!(program.evaluate_bool(&bindings).unwrap());
    }

    #[test]
    fn denied_permission_results_still_consume_reached_rate_limits() {
        let rules = RuleSet::try_new(json!({
            "$rateLimits": {"writes": {"limits": [{"capacity": 2}]}},
            "things": {"allow": {"create":
                "rateLimit.writes.limit('global') && false"
            }}
        }))
        .unwrap();
        let mut bindings = EvaluationBindings::default();
        rules
            .configure_rate_limits(
                Uuid::new_v4(),
                &mut bindings,
                Arc::new(InMemoryRateLimiter::default()),
            )
            .unwrap();
        let program = rules
            .program("things", PermissionAction::Create)
            .unwrap()
            .unwrap();

        assert!(!program.evaluate_bool(&bindings).unwrap());
        assert!(!program.evaluate_bool(&bindings).unwrap());
        assert!(matches!(
            program.evaluate_bool(&bindings),
            Err(PermissionError::RateLimited { .. })
        ));
    }

    #[test]
    fn reset_clears_only_the_requested_bucket() {
        let limiter = InMemoryRateLimiter::default();
        let request = RateLimitRequest {
            app_id: Uuid::new_v4(),
            bucket_name: "consume".to_owned(),
            config: RateLimitBucketConfig::parse(&json!({"limits": [{"capacity": 1}]})).unwrap(),
            bucket_key: json!("first@example.com"),
            tokens: 1,
        };
        assert!(matches!(
            limiter.consume(&request).unwrap(),
            RateLimitDecision::Allowed { .. }
        ));
        assert!(matches!(
            limiter.consume(&request).unwrap(),
            RateLimitDecision::Denied { .. }
        ));
        let mut other = request.clone();
        other.bucket_key = json!("second@example.com");
        assert!(matches!(
            limiter.consume(&other).unwrap(),
            RateLimitDecision::Allowed { .. }
        ));

        limiter.reset(&request).unwrap();
        assert!(matches!(
            limiter.consume(&request).unwrap(),
            RateLimitDecision::Allowed { .. }
        ));
        assert!(matches!(
            limiter.consume(&other).unwrap(),
            RateLimitDecision::Denied { .. }
        ));

        let absent = RateLimitRequest {
            bucket_key: json!("absent@example.com"),
            ..request
        };
        assert!(limiter.reset(&absent).is_ok());
    }

    #[test]
    fn explicit_tokens_apply_atomically_across_every_bucket_limit() {
        let rules = RuleSet::try_new(json!({
            "$rateLimits": {"writes": {"limits": [
                {"capacity": 5},
                {"capacity": 3, "refill": {"type": "interval", "period": "1 hour"}}
            ]}},
            "things": {"allow": {"update": "rateLimit['writes'].limit(auth.id, 2)"}}
        }))
        .unwrap();
        let mut bindings = EvaluationBindings {
            auth: json!({"id": "user-1"}),
            ..EvaluationBindings::default()
        };
        rules
            .configure_rate_limits(
                Uuid::new_v4(),
                &mut bindings,
                Arc::new(InMemoryRateLimiter::default()),
            )
            .unwrap();
        let program = rules
            .program("things", PermissionAction::Update)
            .unwrap()
            .unwrap();
        assert!(program.evaluate_bool(&bindings).unwrap());
        assert!(matches!(
            program.evaluate_bool(&bindings),
            Err(PermissionError::RateLimited { .. })
        ));
    }

    #[test]
    fn reference_retries_do_not_consume_rate_limits_twice() {
        let rules = RuleSet::try_new(json!({
            "$rateLimits": {"reads": {"limits": [{"capacity": 1}]}},
            "things": {"allow": {"view":
                "rateLimit.reads.limit(auth.id) && data.ref('owner') == ['owner']"
            }}
        }))
        .unwrap();
        let program = rules
            .program("things", PermissionAction::View)
            .unwrap()
            .unwrap();
        let mut bindings = EvaluationBindings {
            data: json!({"id": Uuid::new_v4()}),
            data_etype: Some("things".to_owned()),
            auth: json!({"id": "user-1"}),
            ..EvaluationBindings::default()
        };
        rules
            .configure_rate_limits(
                Uuid::new_v4(),
                &mut bindings,
                Arc::new(InMemoryRateLimiter::default()),
            )
            .unwrap();
        let items = [EvaluationItem {
            program: &program,
            bindings: &bindings,
        }];
        let mut refs = RefCache::new();
        let result = evaluate_batch(&items, &mut refs, |requests| {
            Ok(requests
                .iter()
                .cloned()
                .map(|request| (request, vec![json!("owner")]))
                .collect())
        })
        .unwrap();
        assert_eq!(result, [true]);
        assert!(matches!(
            evaluate_batch(&items, &mut refs, |_| Ok(RefCache::new())),
            Err(PermissionError::RateLimited { .. })
        ));
    }
}
