use std::{
    collections::HashMap,
    fs,
    str::FromStr,
    sync::{LazyLock, RwLock},
};

use convert_case::{Case, Casing};
use serde::{Deserialize, Deserializer, Serialize, de::Error as DeError};
use thiserror::Error;
use ts_rs::TS;

use crate::executors::{
    AvailabilityInfo, BaseCodingAgent, CodingAgent, StandardCodingAgentExecutor,
};

/// Return the canonical form for variant keys.
/// – "DEFAULT" is kept as-is  
/// – everything else is converted to SCREAMING_SNAKE_CASE
pub fn canonical_variant_key<S: AsRef<str>>(raw: S) -> String {
    let key = raw.as_ref();
    if key.eq_ignore_ascii_case("DEFAULT") {
        "DEFAULT".to_string()
    } else {
        // Convert to SCREAMING_SNAKE_CASE by first going to snake_case then uppercase
        key.to_case(Case::Snake).to_case(Case::ScreamingSnake)
    }
}

#[derive(Error, Debug)]
pub enum ProfileError {
    #[error("Validation error: {0}")]
    Validation(String),

    #[error(transparent)]
    Io(#[from] std::io::Error),

    #[error(transparent)]
    Serde(#[from] serde_json::Error),

    #[error("No available executor profile")]
    NoAvailableExecutorProfile,
}

/// Result of loading profiles - contains the profiles and optionally a parse error message
#[derive(Clone, Debug, Serialize, Deserialize, TS)]
pub struct ProfileLoadResult {
    pub profiles: ExecutorConfigs,
    /// If profiles file exists but failed to parse, this contains the error message
    pub parse_error: Option<String>,
}

static EXECUTOR_PROFILES_CACHE: LazyLock<RwLock<ExecutorConfigs>> =
    LazyLock::new(|| RwLock::new(ExecutorConfigs::load().profiles));

// Executor-centric profile identifier
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, TS, Hash, Eq)]
pub struct ExecutorProfileId {
    /// The executor type (e.g., "CLAUDE_CODE")
    #[serde(alias = "profile", deserialize_with = "de_base_coding_agent_kebab")]
    // Backwards compatability with ProfileVariantIds, esp stored in DB under ExecutorAction
    pub executor: BaseCodingAgent,
    /// Optional variant name (e.g., "PLAN", "ROUTER")
    #[serde(skip_serializing_if = "Option::is_none")]
    pub variant: Option<String>,
}

// Convert legacy profile/executor names from kebab-case to SCREAMING_SNAKE_CASE, can be deleted 14 days from 3/9/25
fn de_base_coding_agent_kebab<'de, D>(de: D) -> Result<BaseCodingAgent, D::Error>
where
    D: Deserializer<'de>,
{
    let raw = String::deserialize(de)?;
    // kebab-case -> SCREAMING_SNAKE_CASE
    let norm = raw.replace('-', "_").to_ascii_uppercase();
    BaseCodingAgent::from_str(&norm)
        .map_err(|_| D::Error::custom(format!("unknown executor '{raw}' (normalized to '{norm}')")))
}

impl ExecutorProfileId {
    /// Create a new executor profile ID with default variant
    pub fn new(executor: BaseCodingAgent) -> Self {
        Self {
            executor,
            variant: None,
        }
    }

    /// Create a new executor profile ID with specific variant
    pub fn with_variant(executor: BaseCodingAgent, variant: String) -> Self {
        Self {
            executor,
            variant: Some(variant),
        }
    }

    /// Get cache key for this executor profile
    pub fn cache_key(&self) -> String {
        match &self.variant {
            Some(variant) => format!("{}:{}", self.executor, variant),
            None => self.executor.clone().to_string(),
        }
    }
}

impl std::fmt::Display for ExecutorProfileId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.variant {
            Some(variant) => write!(f, "{}:{}", self.executor, variant),
            None => write!(f, "{}", self.executor),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, TS)]
pub struct ExecutorConfig {
    #[serde(flatten)]
    pub configurations: HashMap<String, CodingAgent>,
}

impl ExecutorConfig {
    /// Get variant configuration by name, or None if not found
    pub fn get_variant(&self, variant: &str) -> Option<&CodingAgent> {
        self.configurations.get(variant)
    }

    /// Get the default configuration for this executor
    pub fn get_default(&self) -> Option<&CodingAgent> {
        self.configurations.get("DEFAULT")
    }

    /// Create a new executor profile with just a default configuration
    pub fn new_with_default(default_config: CodingAgent) -> Self {
        let mut configurations = HashMap::new();
        configurations.insert("DEFAULT".to_string(), default_config);
        Self { configurations }
    }

    /// Add or update a variant configuration
    pub fn set_variant(
        &mut self,
        variant_name: String,
        config: CodingAgent,
    ) -> Result<(), &'static str> {
        let key = canonical_variant_key(&variant_name);
        if key == "DEFAULT" {
            return Err(
                "Cannot override 'DEFAULT' variant using set_variant, use set_default instead",
            );
        }
        self.configurations.insert(key, config);
        Ok(())
    }

    /// Set the default configuration
    pub fn set_default(&mut self, config: CodingAgent) {
        self.configurations.insert("DEFAULT".to_string(), config);
    }

    /// Get all variant names (excluding "DEFAULT")
    pub fn variant_names(&self) -> Vec<&String> {
        self.configurations
            .keys()
            .filter(|k| *k != "DEFAULT")
            .collect()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, TS)]
pub struct ExecutorConfigs {
    pub executors: HashMap<BaseCodingAgent, ExecutorConfig>,
}

impl ExecutorConfigs {
    /// Normalise all variant keys in-place
    fn canonicalise(&mut self) {
        for profile in self.executors.values_mut() {
            let mut replacements = Vec::new();
            for key in profile.configurations.keys().cloned().collect::<Vec<_>>() {
                let canon = canonical_variant_key(&key);
                if canon != key {
                    replacements.push((key, canon));
                }
            }
            for (old, new) in replacements {
                if let Some(cfg) = profile.configurations.remove(&old) {
                    // If both lowercase and canonical forms existed, keep canonical one
                    profile.configurations.entry(new).or_insert(cfg);
                }
            }
        }
    }

    /// Get cached executor profiles
    pub fn get_cached() -> ExecutorConfigs {
        EXECUTOR_PROFILES_CACHE.read().unwrap().clone()
    }

    /// Reload executor profiles cache
    pub fn reload() {
        let mut cache = EXECUTOR_PROFILES_CACHE.write().unwrap();
        *cache = Self::load().profiles;
    }

    /// Load executor profiles from file or defaults.
    /// If profiles.json exists, reads it directly; otherwise creates it from defaults.
    pub fn load() -> ProfileLoadResult {
        let profiles_path = workspace_utils::assets::profiles_path();

        if profiles_path.exists() {
            match fs::read_to_string(&profiles_path) {
                Ok(content) => {
                    tracing::info!("Profiles file loaded from {:?}", profiles_path);
                    match serde_json::from_str::<ExecutorConfigs>(&content) {
                        Ok(mut profiles) => {
                            profiles.canonicalise();
                            let defaults = Self::from_defaults();
                            if profiles
                                .inject_missing_executor_configs(&defaults, &[BaseCodingAgent::Pi])
                            {
                                match serde_json::to_string_pretty(&profiles) {
                                    Ok(updated_content) => {
                                        if let Err(err) = fs::write(&profiles_path, updated_content)
                                        {
                                            tracing::warn!(
                                                "Failed to persist injected executor profiles: {}",
                                                err
                                            );
                                        }
                                    }
                                    Err(err) => {
                                        tracing::warn!(
                                            "Failed to serialize injected executor profiles: {}",
                                            err
                                        );
                                    }
                                }
                            }
                            ProfileLoadResult {
                                profiles,
                                parse_error: None,
                            }
                        }
                        Err(e) => {
                            let error = format!("Failed to parse profiles: {}", e);
                            tracing::warn!("Profiles parse failed: {}, using default", error);
                            let mut defaults = Self::from_defaults();
                            defaults.canonicalise();
                            ProfileLoadResult {
                                profiles: defaults,
                                parse_error: Some(error),
                            }
                        }
                    }
                }
                Err(e) => {
                    tracing::warn!("Failed to read profiles file: {}, using default", e);
                    let mut defaults = Self::from_defaults();
                    defaults.canonicalise();
                    ProfileLoadResult {
                        profiles: defaults,
                        parse_error: None,
                    }
                }
            }
        } else {
            // No profiles.json found, create it from defaults
            tracing::info!(
                "No profiles.json found, creating from defaults at {:?}",
                profiles_path
            );
            let mut defaults = Self::from_defaults();
            defaults.canonicalise();
            let default_content = serde_json::to_string_pretty(&defaults)
                .expect("Failed to serialize default profiles");
            if let Err(e) = fs::write(&profiles_path, &default_content) {
                tracing::warn!("Failed to write default profiles.json: {}", e);
            }
            ProfileLoadResult {
                profiles: defaults,
                parse_error: None,
            }
        }
    }

    /// Save user profile to file (saves the complete configuration)
    pub fn save(&mut self) -> Result<(), ProfileError> {
        let profiles_path = workspace_utils::assets::profiles_path();

        // Canonicalise keys
        self.canonicalise();

        // Validate the result
        Self::validate_merged(self)?;

        // Write directly to file
        let content = serde_json::to_string_pretty(self)?;
        fs::write(&profiles_path, content)?;

        tracing::info!("Saved profiles to {:?}", profiles_path);
        Ok(())
    }

    /// Validate that profiles are consistent and valid
    fn validate_merged(merged: &Self) -> Result<(), ProfileError> {
        for (executor_key, profile) in &merged.executors {
            // Ensure default configuration exists
            let default_config = profile.configurations.get("DEFAULT").ok_or_else(|| {
                ProfileError::Validation(format!(
                    "Executor '{executor_key}' is missing required 'default' configuration"
                ))
            })?;

            // Validate that the default agent type matches the executor key
            if BaseCodingAgent::from(default_config) != *executor_key {
                return Err(ProfileError::Validation(format!(
                    "Executor key '{executor_key}' does not match the agent variant '{default_config}'"
                )));
            }

            // Ensure configuration names don't conflict with reserved words
            for config_name in profile.configurations.keys() {
                if config_name.starts_with("__") {
                    return Err(ProfileError::Validation(format!(
                        "Configuration name '{config_name}' is reserved (starts with '__')"
                    )));
                }
            }
        }
        Ok(())
    }

    /// Load from the embedded default profiles
    pub fn from_defaults() -> Self {
        let default_profiles_bytes = workspace_utils::assets::default_profiles();
        let default_profiles_str = String::from_utf8(default_profiles_bytes)
            .expect("default_profiles.json is not valid UTF-8");
        serde_json::from_str(&default_profiles_str).unwrap_or_else(|e| {
            tracing::error!("Failed to parse embedded default_profiles.json: {}", e);
            panic!("Default profiles JSON is invalid")
        })
    }

    fn inject_missing_executor_configs(
        &mut self,
        defaults: &Self,
        executors: &[BaseCodingAgent],
    ) -> bool {
        let mut changed = false;
        for executor in executors {
            let Some(default_executor_config) = defaults.executors.get(executor) else {
                continue;
            };

            match self.executors.get_mut(executor) {
                Some(existing_executor_config) => {
                    for (variant, config) in &default_executor_config.configurations {
                        if !existing_executor_config
                            .configurations
                            .contains_key(variant)
                        {
                            existing_executor_config
                                .configurations
                                .insert(variant.clone(), config.clone());
                            changed = true;
                        }
                    }
                }
                None => {
                    self.executors
                        .insert(executor.clone(), default_executor_config.clone());
                    changed = true;
                }
            }
        }
        changed
    }

    pub fn get_coding_agent(&self, executor_profile_id: &ExecutorProfileId) -> Option<CodingAgent> {
        self.executors
            .get(&executor_profile_id.executor)
            .and_then(|executor| {
                executor.get_variant(
                    &executor_profile_id
                        .variant
                        .clone()
                        .unwrap_or("DEFAULT".to_string()),
                )
            })
            .cloned()
    }

    pub fn get_coding_agent_or_default(
        &self,
        executor_profile_id: &ExecutorProfileId,
    ) -> CodingAgent {
        self.get_coding_agent(executor_profile_id)
            .unwrap_or_else(|| {
                let mut default_executor_profile_id = executor_profile_id.clone();
                default_executor_profile_id.variant = Some("DEFAULT".to_string());
                self.get_coding_agent(&default_executor_profile_id)
                    .expect("No default variant found")
            })
    }
    pub async fn get_recommended_executor_profile(
        &self,
    ) -> Result<ExecutorProfileId, ProfileError> {
        let mut agents_with_info: Vec<(BaseCodingAgent, AvailabilityInfo)> = Vec::new();

        for &base_agent in self.executors.keys() {
            let profile_id = ExecutorProfileId::new(base_agent);
            if let Some(coding_agent) = self.get_coding_agent(&profile_id) {
                let info = coding_agent.get_availability_info();
                if info.is_available() {
                    agents_with_info.push((base_agent, info));
                }
            }
        }

        if agents_with_info.is_empty() {
            return Err(ProfileError::NoAvailableExecutorProfile);
        }

        agents_with_info.sort_by(|a, b| {
            use crate::executors::AvailabilityInfo;
            match (&a.1, &b.1) {
                // Both have login detected - compare timestamps (most recent first)
                (
                    AvailabilityInfo::LoginDetected {
                        last_auth_timestamp: time_a,
                    },
                    AvailabilityInfo::LoginDetected {
                        last_auth_timestamp: time_b,
                    },
                ) => time_b.cmp(time_a),
                // LoginDetected > InstallationFound
                (AvailabilityInfo::LoginDetected { .. }, AvailabilityInfo::InstallationFound) => {
                    std::cmp::Ordering::Less
                }
                (AvailabilityInfo::InstallationFound, AvailabilityInfo::LoginDetected { .. }) => {
                    std::cmp::Ordering::Greater
                }
                // LoginDetected > NotFound
                (AvailabilityInfo::LoginDetected { .. }, AvailabilityInfo::NotFound) => {
                    std::cmp::Ordering::Less
                }
                (AvailabilityInfo::NotFound, AvailabilityInfo::LoginDetected { .. }) => {
                    std::cmp::Ordering::Greater
                }
                // InstallationFound > NotFound
                (AvailabilityInfo::InstallationFound, AvailabilityInfo::NotFound) => {
                    std::cmp::Ordering::Less
                }
                (AvailabilityInfo::NotFound, AvailabilityInfo::InstallationFound) => {
                    std::cmp::Ordering::Greater
                }
                // Same state - equal
                _ => std::cmp::Ordering::Equal,
            }
        });

        let selected = agents_with_info[0].0;
        tracing::info!("Recommended executor: {}", selected);
        Ok(ExecutorProfileId::new(selected))
    }
}

pub fn to_default_variant(id: &ExecutorProfileId) -> ExecutorProfileId {
    ExecutorProfileId {
        executor: id.executor,
        variant: None,
    }
}
