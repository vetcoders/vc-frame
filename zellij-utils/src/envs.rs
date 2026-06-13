/// Uniformly operates VC_FRAME* environment variables with ZELLIJ* compatibility.
use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, HashMap},
    env::{set_var, var},
};

use std::fmt;

pub const ZELLIJ_ENV_KEY: &str = "ZELLIJ";
pub const VC_FRAME_ENV_KEY: &str = "VC_FRAME";
pub fn get_zellij() -> Result<String> {
    aliased_var(VC_FRAME_ENV_KEY, ZELLIJ_ENV_KEY)
}
pub fn set_zellij(v: String) {
    set_process_env(ZELLIJ_ENV_KEY, &v);
    set_process_env(VC_FRAME_ENV_KEY, v);
}

pub const SESSION_NAME_ENV_KEY: &str = "ZELLIJ_SESSION_NAME";
pub const VC_FRAME_SESSION_NAME_ENV_KEY: &str = "VC_FRAME_SESSION_NAME";

pub fn get_session_name() -> Result<String> {
    aliased_var(VC_FRAME_SESSION_NAME_ENV_KEY, SESSION_NAME_ENV_KEY)
}

pub fn set_session_name(v: String) {
    set_process_env(SESSION_NAME_ENV_KEY, &v);
    set_process_env(VC_FRAME_SESSION_NAME_ENV_KEY, v);
}

pub const SOCKET_DIR_ENV_KEY: &str = "ZELLIJ_SOCKET_DIR";
pub const VC_FRAME_SOCKET_DIR_ENV_KEY: &str = "VC_FRAME_SOCKET_DIR";
pub fn get_socket_dir() -> Result<String> {
    aliased_var(VC_FRAME_SOCKET_DIR_ENV_KEY, SOCKET_DIR_ENV_KEY)
}

pub const PANE_ID_ENV_KEY: &str = "ZELLIJ_PANE_ID";
pub const VC_FRAME_PANE_ID_ENV_KEY: &str = "VC_FRAME_PANE_ID";
pub fn get_pane_id() -> Result<String> {
    aliased_var(VC_FRAME_PANE_ID_ENV_KEY, PANE_ID_ENV_KEY)
}

const ENV_ALIASES: &[(&str, &str)] = &[
    (VC_FRAME_ENV_KEY, ZELLIJ_ENV_KEY),
    (VC_FRAME_SESSION_NAME_ENV_KEY, SESSION_NAME_ENV_KEY),
    (VC_FRAME_SOCKET_DIR_ENV_KEY, SOCKET_DIR_ENV_KEY),
    (VC_FRAME_PANE_ID_ENV_KEY, PANE_ID_ENV_KEY),
    ("VC_FRAME_CONFIG_FILE", "ZELLIJ_CONFIG_FILE"),
    ("VC_FRAME_CONFIG_DIR", "ZELLIJ_CONFIG_DIR"),
    ("VC_FRAME_LAYOUT_DIR", "ZELLIJ_LAYOUT_DIR"),
    ("VC_FRAME_AUTO_ATTACH", "ZELLIJ_AUTO_ATTACH"),
    ("VC_FRAME_AUTO_EXIT", "ZELLIJ_AUTO_EXIT"),
];

pub fn normalize_vc_frame_env_aliases() {
    for (primary, fallback) in ENV_ALIASES {
        mirror_env_alias(primary, fallback);
    }
}

fn aliased_var(primary: &str, fallback: &str) -> Result<String> {
    Ok(var(primary).or_else(|_| var(fallback))?)
}

fn mirror_env_alias(primary: &str, fallback: &str) {
    if let Ok(value) = var(primary) {
        set_process_env(fallback, value);
    } else if let Ok(value) = var(fallback) {
        set_process_env(primary, value);
    }
}

fn is_known_alias(key: &str) -> bool {
    ENV_ALIASES
        .iter()
        .any(|(primary, fallback)| key == *primary || key == *fallback)
}

/// Manage ENVIRONMENT VARIABLES from the configuration and the layout files
#[derive(Default, Clone, PartialEq, Serialize, Deserialize)]
pub struct EnvironmentVariables {
    env: HashMap<String, String>,
}

impl fmt::Debug for EnvironmentVariables {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        let mut stable_sorted = BTreeMap::new();
        for (env_var_name, env_var_value) in self.env.iter() {
            stable_sorted.insert(env_var_name, env_var_value);
        }
        write!(f, "{:#?}", stable_sorted)
    }
}

impl EnvironmentVariables {
    /// Merges two structs, keys from `other` supersede keys from `self`
    pub fn merge(&self, other: Self) -> Self {
        let mut env = self.clone();
        env.env.extend(other.env);
        env
    }
    pub fn from_data(data: HashMap<String, String>) -> Self {
        EnvironmentVariables { env: data }
    }
    /// Set all the ENVIRONMENT VARIABLES, that are configured
    /// in the configuration and layout files
    pub fn set_vars(&self) {
        for (k, v) in &self.env {
            if !is_known_alias(k) {
                set_process_env(k, v);
            }
        }
        for (primary, fallback) in ENV_ALIASES {
            if let Some(value) = self.env.get(*primary).or_else(|| self.env.get(*fallback)) {
                set_process_env(fallback, value);
                set_process_env(primary, value);
            }
        }
    }
    pub fn inner(&self) -> &HashMap<String, String> {
        &self.env
    }
}

fn set_process_env<K: AsRef<std::ffi::OsStr>, V: AsRef<std::ffi::OsStr>>(key: K, value: V) {
    // SAFETY: VC Frame applies these process-wide variables during startup/configuration before
    // handing control to worker threads that read them.
    unsafe {
        set_var(key, value);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Mutex, OnceLock};

    fn env_lock() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(())).lock().unwrap()
    }

    fn remove_test_env(key: &str) {
        unsafe {
            std::env::remove_var(key);
        }
    }

    fn remove_all_alias_envs() {
        for (primary, fallback) in ENV_ALIASES {
            remove_test_env(primary);
            remove_test_env(fallback);
        }
    }

    #[test]
    fn session_name_prefers_vc_frame_and_falls_back_to_zellij() {
        let _guard = env_lock();
        remove_test_env(VC_FRAME_SESSION_NAME_ENV_KEY);
        remove_test_env(SESSION_NAME_ENV_KEY);

        set_process_env(SESSION_NAME_ENV_KEY, "legacy-session");
        assert_eq!(get_session_name().unwrap(), "legacy-session");

        set_process_env(VC_FRAME_SESSION_NAME_ENV_KEY, "vc-session");
        assert_eq!(get_session_name().unwrap(), "vc-session");

        remove_test_env(VC_FRAME_SESSION_NAME_ENV_KEY);
        remove_test_env(SESSION_NAME_ENV_KEY);
    }

    #[test]
    fn pane_id_prefers_vc_frame_and_falls_back_to_zellij() {
        let _guard = env_lock();
        remove_test_env(VC_FRAME_PANE_ID_ENV_KEY);
        remove_test_env(PANE_ID_ENV_KEY);

        set_process_env(PANE_ID_ENV_KEY, "7");
        assert_eq!(get_pane_id().unwrap(), "7");

        set_process_env(VC_FRAME_PANE_ID_ENV_KEY, "8");
        assert_eq!(get_pane_id().unwrap(), "8");

        remove_test_env(VC_FRAME_PANE_ID_ENV_KEY);
        remove_test_env(PANE_ID_ENV_KEY);
    }

    #[test]
    fn normalization_mirrors_config_alias_for_clap_env_lookup() {
        let _guard = env_lock();
        remove_test_env("VC_FRAME_CONFIG_FILE");
        remove_test_env("ZELLIJ_CONFIG_FILE");

        set_process_env("VC_FRAME_CONFIG_FILE", "/tmp/vc-frame.kdl");
        normalize_vc_frame_env_aliases();
        assert_eq!(
            std::env::var("ZELLIJ_CONFIG_FILE").unwrap(),
            "/tmp/vc-frame.kdl"
        );

        remove_test_env("VC_FRAME_CONFIG_FILE");
        remove_test_env("ZELLIJ_CONFIG_FILE");
    }

    #[test]
    fn set_vars_dual_exports_known_aliases_with_vc_frame_canonical() {
        let _guard = env_lock();
        remove_all_alias_envs();

        let vars = EnvironmentVariables::from_data(HashMap::from([
            (PANE_ID_ENV_KEY.to_string(), "legacy-pane".to_string()),
            (
                VC_FRAME_SESSION_NAME_ENV_KEY.to_string(),
                "canonical-session".to_string(),
            ),
            (
                SESSION_NAME_ENV_KEY.to_string(),
                "legacy-session".to_string(),
            ),
            ("CUSTOM_ONLY".to_string(), "kept".to_string()),
        ]));
        vars.set_vars();

        assert_eq!(std::env::var(PANE_ID_ENV_KEY).unwrap(), "legacy-pane");
        assert_eq!(
            std::env::var(VC_FRAME_PANE_ID_ENV_KEY).unwrap(),
            "legacy-pane"
        );
        assert_eq!(
            std::env::var(SESSION_NAME_ENV_KEY).unwrap(),
            "canonical-session"
        );
        assert_eq!(
            std::env::var(VC_FRAME_SESSION_NAME_ENV_KEY).unwrap(),
            "canonical-session"
        );
        assert_eq!(std::env::var("CUSTOM_ONLY").unwrap(), "kept");

        remove_all_alias_envs();
        remove_test_env("CUSTOM_ONLY");
    }
}
