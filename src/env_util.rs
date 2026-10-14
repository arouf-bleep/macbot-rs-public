//! Shared environment-variable parsing helpers.
//! Centralised to avoid duplicating parsing logic across modules.

pub fn env_bool(name: &str, default: bool) -> bool {
    std::env::var(name)
        .ok()
        .and_then(|v| {
            let x = v.trim().to_ascii_lowercase();
            match x.as_str() {
                "1" | "true" | "yes" | "on" => Some(true),
                "0" | "false" | "no" | "off" => Some(false),
                _ => None,
            }
        })
        .unwrap_or(default)
}

pub fn env_u64(name: &str, default: u64) -> u64 {
    std::env::var(name)
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .unwrap_or(default)
}

pub fn env_f64(name: &str, default: f64) -> f64 {
    std::env::var(name)
        .ok()
        .and_then(|v| v.trim().parse::<f64>().ok())
        .filter(|value| value.is_finite())
        .unwrap_or(default)
}

/// Serialize tests that temporarily mutate the process-wide environment.
///
/// Rust runs test modules in parallel, but environment variables belong to
/// the whole process. One shared lock prevents an unrelated test from reading
/// another test's temporary value.
#[cfg(test)]
pub(crate) fn test_env_lock() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();
    LOCK.get_or_init(|| std::sync::Mutex::new(()))
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numeric_environment_requires_finite_values_and_accepts_whitespace() {
        let _guard = test_env_lock();
        const KEY: &str = "MACBOT_TEST_ENV_NUMERIC_PARSE";
        let previous = std::env::var_os(KEY);
        for invalid in ["NaN", "inf", "-inf", "1e999", "nonsense"] {
            std::env::set_var(KEY, invalid);
            assert_eq!(env_f64(KEY, 3.0), 3.0, "{invalid}");
        }
        std::env::set_var(KEY, " 2.5 ");
        assert_eq!(env_f64(KEY, 3.0), 2.5);
        std::env::set_var(KEY, " 12 ");
        assert_eq!(env_u64(KEY, 3), 12);
        if let Some(previous) = previous {
            std::env::set_var(KEY, previous);
        } else {
            std::env::remove_var(KEY);
        }
    }
}
