use std::{
    collections::{BTreeSet, HashMap, HashSet},
    path::{Component, Path, PathBuf},
};

use axum::{
    Json, Router,
    extract::Query,
    response::Json as ResponseJson,
    routing::{get, post},
};
use executors::executors::BaseCodingAgent;
use git2::Repository;
use serde::{Deserialize, Serialize};
use ts_rs::TS;
use url::Url;
use utils::{response::ApiResponse, shell::resolve_executable_path_blocking};
use uuid::Uuid;

use crate::{DeploymentImpl, error::ApiError};

const SKILL_MD_FILE: &str = "SKILL.md";

pub fn router() -> Router<DeploymentImpl> {
    Router::new()
        .route("/skills", get(get_skills))
        .route(
            "/skills/links",
            get(get_skill_links).post(link_skills).delete(unlink_skill),
        )
        .route("/skills/import", post(import_skills))
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
pub struct SkillInfo {
    pub name: String,
    pub description: String,
    pub path: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS, PartialEq, Eq)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
#[ts(use_ts_enum)]
pub enum SkillLinkState {
    Linked,
    NotLinked,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
pub struct AgentSkillLinkInfo {
    pub skill_name: String,
    pub description: String,
    pub state: SkillLinkState,
    pub agent_path: String,
    pub canonical_path: String,
    #[serde(default)]
    pub is_legacy: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
pub struct GetSkillsResponse {
    pub skills: Vec<SkillInfo>,
    pub canonical_dir: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
pub struct GetSkillLinksResponse {
    pub executor: BaseCodingAgent,
    pub agent_dir: String,
    pub links: Vec<AgentSkillLinkInfo>,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
pub struct LinkSkillsBody {
    pub skill_names: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
pub struct ImportSkillsBody {
    pub source: String,
    pub git_ref: Option<String>,
    pub subpath: Option<String>,
    pub skill_filter: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
pub struct ImportSkillsResponse {
    pub imported: Vec<SkillInfo>,
    pub skipped: Vec<String>,
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
struct SkillLinksQuery {
    executor: BaseCodingAgent,
}

#[derive(Debug, Clone, Deserialize)]
struct UnlinkSkillQuery {
    executor: BaseCodingAgent,
    skill_name: String,
}

#[derive(Debug, Clone)]
struct CanonicalSkill {
    folder_name: String,
    path: PathBuf,
    info: SkillInfo,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ParsedImportSource {
    clone_url: String,
    repo_name: String,
    git_ref: Option<String>,
    subpath: Option<String>,
}

async fn get_skills() -> Result<ResponseJson<ApiResponse<GetSkillsResponse>>, ApiError> {
    let canonical_dir = ensure_canonical_skills_dir()?;
    let skills = discover_skills_in_dir(&canonical_dir)?;

    Ok(ResponseJson(ApiResponse::success(GetSkillsResponse {
        skills: skills.into_iter().map(|s| s.info).collect(),
        canonical_dir: canonical_dir.to_string_lossy().to_string(),
    })))
}

async fn get_skill_links(
    Query(query): Query<SkillLinksQuery>,
) -> Result<ResponseJson<ApiResponse<GetSkillLinksResponse>>, ApiError> {
    let canonical_dir = ensure_canonical_skills_dir()?;
    let canonical_skills = discover_skills_in_dir(&canonical_dir)?;
    let agent_dir = ensure_agent_skills_dir(query.executor)?;
    let response = build_links_response(query.executor, &agent_dir, &canonical_skills)?;

    Ok(ResponseJson(ApiResponse::success(response)))
}

async fn link_skills(
    Query(query): Query<SkillLinksQuery>,
    Json(payload): Json<LinkSkillsBody>,
) -> Result<ResponseJson<ApiResponse<GetSkillLinksResponse>>, ApiError> {
    let canonical_dir = ensure_canonical_skills_dir()?;
    let canonical_skills = discover_skills_in_dir(&canonical_dir)?;
    let canonical_by_folder: HashMap<String, &CanonicalSkill> = canonical_skills
        .iter()
        .map(|skill| (skill.folder_name.clone(), skill))
        .collect();

    let requested_skill_names = normalize_skill_names(payload.skill_names)?;
    if requested_skill_names.is_empty() {
        return Err(ApiError::BadRequest(
            "skill_names must contain at least one skill".to_string(),
        ));
    }

    let agent_dir = ensure_agent_skills_dir(query.executor)?;
    let mut links_to_create: Vec<(PathBuf, PathBuf)> = Vec::new();

    for skill_name in &requested_skill_names {
        let canonical_skill = canonical_by_folder.get(skill_name).ok_or_else(|| {
            ApiError::BadRequest(format!(
                "Skill `{skill_name}` was not found in the canonical skills directory"
            ))
        })?;

        let link_path = agent_dir.join(skill_name);
        if link_path == canonical_skill.path {
            continue;
        }
        if is_matching_symlink(&link_path, &canonical_skill.path)? {
            continue;
        }
        if path_entry_exists(&link_path) {
            return Err(ApiError::Conflict(format!(
                "Cannot link skill `{skill_name}` because `{}` already exists. Unlink it first.",
                link_path.display()
            )));
        }
        links_to_create.push((link_path, canonical_skill.path.clone()));
    }

    for (link_path, canonical_path) in links_to_create {
        create_directory_symlink(&canonical_path, &link_path)?;
    }

    let response = build_links_response(query.executor, &agent_dir, &canonical_skills)?;
    Ok(ResponseJson(ApiResponse::success(response)))
}

async fn unlink_skill(
    Query(query): Query<UnlinkSkillQuery>,
) -> Result<ResponseJson<ApiResponse<GetSkillLinksResponse>>, ApiError> {
    let skill_name = normalize_skill_name(&query.skill_name)?;
    let agent_dir = ensure_agent_skills_dir(query.executor)?;
    let canonical_dir = ensure_canonical_skills_dir()?;
    if agent_dir == canonical_dir {
        return Err(ApiError::BadRequest(format!(
            "Executor `{}` uses the canonical skills directory directly and does not support unlink",
            query.executor
        )));
    }
    unlink_skill_path(&agent_dir.join(&skill_name))?;

    let canonical_skills = discover_skills_in_dir(&canonical_dir)?;
    let response = build_links_response(query.executor, &agent_dir, &canonical_skills)?;
    Ok(ResponseJson(ApiResponse::success(response)))
}

async fn import_skills(
    Json(payload): Json<ImportSkillsBody>,
) -> Result<ResponseJson<ApiResponse<ImportSkillsResponse>>, ApiError> {
    let canonical_dir = ensure_canonical_skills_dir()?;
    let source = parse_import_source(&payload)?;
    let temp_dir = create_temp_import_dir()?;
    let clone_dir = temp_dir.join(&source.repo_name);
    let root_skill_folder_name = preferred_root_skill_folder_name(&source, &payload);

    let import_result = (|| -> Result<ImportSkillsResponse, ApiError> {
        clone_repository(&source.clone_url, &clone_dir)?;

        if let Some(git_ref) = source.git_ref.as_deref() {
            checkout_git_ref(&clone_dir, git_ref)?;
        }

        import_skills_from_repo_root(
            &clone_dir,
            &canonical_dir,
            source.subpath.as_deref(),
            payload.skill_filter.as_deref(),
            Some(root_skill_folder_name.as_str()),
        )
    })();

    let cleanup_result = std::fs::remove_dir_all(&temp_dir);

    match import_result {
        Ok(mut response) => {
            if let Err(err) = cleanup_result {
                response.warnings.push(format!(
                    "Failed to clean temporary import directory `{}`: {err}",
                    temp_dir.display()
                ));
            }
            Ok(ResponseJson(ApiResponse::success(response)))
        }
        Err(err) => {
            if let Err(cleanup_err) = cleanup_result {
                tracing::warn!(
                    "Failed to clean temporary import directory `{}` after error: {}",
                    temp_dir.display(),
                    cleanup_err
                );
            }
            Err(err)
        }
    }
}

fn parse_import_source(body: &ImportSkillsBody) -> Result<ParsedImportSource, ApiError> {
    parse_import_source_values(
        &body.source,
        body.git_ref.as_deref(),
        body.subpath.as_deref(),
    )
}

fn parse_import_source_values(
    source: &str,
    git_ref: Option<&str>,
    subpath: Option<&str>,
) -> Result<ParsedImportSource, ApiError> {
    let source = source.trim();
    if source.is_empty() {
        return Err(ApiError::BadRequest(
            "Import source is required".to_string(),
        ));
    }

    let mut inferred_git_ref: Option<String> = None;
    let mut inferred_subpath: Option<String> = None;

    let clone_url = if let Some((owner, repo)) = parse_owner_repo(source) {
        format!("https://github.com/{owner}/{repo}.git")
    } else if let Ok(url) = Url::parse(source) {
        if is_github_host(url.host_str()) {
            let parsed = parse_github_source_url(&url)?;
            inferred_git_ref = parsed.git_ref;
            inferred_subpath = parsed.subpath;
            parsed.clone_url
        } else {
            source.to_string()
        }
    } else if looks_like_git_ssh_url(source) {
        source.to_string()
    } else {
        return Err(ApiError::BadRequest(
            "Unsupported source format. Use owner/repo, a GitHub URL, or a git URL.".to_string(),
        ));
    };

    let git_ref = normalize_optional_string(git_ref).or(inferred_git_ref);
    let subpath = normalize_optional_string(subpath).or(inferred_subpath);
    let subpath = match subpath {
        Some(value) => Some(normalize_relative_path(&value)?),
        None => None,
    };
    let repo_name = repo_name_from_clone_url(&clone_url).unwrap_or_else(|| "repo".to_string());

    Ok(ParsedImportSource {
        clone_url,
        repo_name,
        git_ref,
        subpath,
    })
}

fn repo_name_from_clone_url(clone_url: &str) -> Option<String> {
    let candidate = if let Ok(url) = Url::parse(clone_url) {
        url.path_segments().and_then(|segments| {
            segments
                .filter(|segment| !segment.is_empty())
                .next_back()
                .map(str::to_string)
        })
    } else {
        clone_url
            .trim()
            .trim_end_matches('/')
            .rsplit(['/', ':'])
            .next()
            .map(str::to_string)
    }?;

    let trimmed = candidate.trim_end_matches(".git").trim();
    if trimmed.is_empty() {
        return None;
    }
    normalize_skill_name(trimmed).ok()
}

fn preferred_root_skill_folder_name(
    source: &ParsedImportSource,
    payload: &ImportSkillsBody,
) -> String {
    if let Some(filter_name) = path_segment_name(payload.skill_filter.as_deref(), true) {
        return filter_name;
    }
    if let Some(subpath_name) = path_segment_name(source.subpath.as_deref(), false) {
        return subpath_name;
    }
    source.repo_name.clone()
}

fn path_segment_name(path_value: Option<&str>, require_path_separator: bool) -> Option<String> {
    let raw = normalize_optional_string(path_value)?;
    if require_path_separator && !raw.contains('/') && !raw.contains('\\') {
        return None;
    }

    let normalized = raw.replace('\\', "/");
    let candidate = normalized.rsplit('/').next()?;
    let trimmed = candidate.trim();
    if trimmed.is_empty() {
        return None;
    }
    normalize_skill_name(trimmed).ok()
}

fn parse_owner_repo(source: &str) -> Option<(String, String)> {
    let mut parts = source.split('/');
    let owner = parts.next()?;
    let repo = parts.next()?;
    if parts.next().is_some() {
        return None;
    }
    if !is_valid_repo_segment(owner) || !is_valid_repo_segment(repo) {
        return None;
    }
    Some((owner.to_string(), repo.to_string()))
}

fn is_valid_repo_segment(segment: &str) -> bool {
    !segment.is_empty()
        && segment
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.')
}

fn is_github_host(host: Option<&str>) -> bool {
    matches!(host, Some("github.com" | "www.github.com"))
}

fn looks_like_git_ssh_url(source: &str) -> bool {
    source.contains('@') && source.contains(':') && source.contains('/')
}

#[derive(Debug, Clone)]
struct ParsedGithubSourceUrl {
    clone_url: String,
    git_ref: Option<String>,
    subpath: Option<String>,
}

fn parse_github_source_url(url: &Url) -> Result<ParsedGithubSourceUrl, ApiError> {
    let parts: Vec<&str> = url
        .path_segments()
        .map(|segments| segments.filter(|segment| !segment.is_empty()).collect())
        .unwrap_or_default();

    if parts.len() < 2 {
        return Err(ApiError::BadRequest(
            "GitHub URL must include owner and repository".to_string(),
        ));
    }

    let owner = parts[0];
    let repo = parts[1].trim_end_matches(".git");

    if !is_valid_repo_segment(owner) || !is_valid_repo_segment(repo) {
        return Err(ApiError::BadRequest(
            "GitHub URL has an invalid owner or repository name".to_string(),
        ));
    }

    let mut git_ref = None;
    let mut subpath = None;

    if parts.len() > 3 && matches!(parts[2], "tree" | "blob") {
        git_ref = normalize_optional_string(Some(parts[3]));
        if parts.len() > 4 {
            subpath = Some(parts[4..].join("/"));
        }
    } else if parts.len() > 2 {
        subpath = Some(parts[2..].join("/"));
    }

    Ok(ParsedGithubSourceUrl {
        clone_url: format!("https://github.com/{owner}/{repo}.git"),
        git_ref,
        subpath,
    })
}

fn clone_repository(clone_url: &str, clone_dir: &Path) -> Result<(), ApiError> {
    match Repository::clone(clone_url, clone_dir) {
        Ok(_) => Ok(()),
        Err(libgit2_err) => {
            if !should_retry_clone_with_git_cli(&libgit2_err) {
                return Err(ApiError::BadRequest(format!(
                    "Failed to clone source `{clone_url}`: {libgit2_err}"
                )));
            }

            tracing::warn!(
                "libgit2 clone failed for `{}` with `{}`; retrying with system git",
                clone_url,
                libgit2_err
            );

            cleanup_partial_clone_dir(clone_dir)?;
            clone_with_git_cli(clone_url, clone_dir).map_err(|cli_err| {
                ApiError::BadRequest(format!("Failed to clone source `{clone_url}`: {cli_err}"))
            })
        }
    }
}

fn should_retry_clone_with_git_cli(err: &git2::Error) -> bool {
    if matches!(
        err.class(),
        git2::ErrorClass::Ssl | git2::ErrorClass::Net | git2::ErrorClass::Ssh
    ) {
        return true;
    }

    let message = err.message().to_ascii_lowercase();
    message.contains("tls")
        || message.contains("ssl")
        || message.contains("certificate")
        || message.contains("http")
        || message.contains("transport")
}

fn cleanup_partial_clone_dir(clone_dir: &Path) -> Result<(), ApiError> {
    if !path_entry_exists(clone_dir) {
        return Ok(());
    }

    let metadata = std::fs::symlink_metadata(clone_dir)?;
    if metadata.is_dir() {
        std::fs::remove_dir_all(clone_dir)?;
    } else {
        std::fs::remove_file(clone_dir)?;
    }
    Ok(())
}

fn clone_with_git_cli(clone_url: &str, clone_dir: &Path) -> Result<(), String> {
    let git = resolve_executable_path_blocking("git")
        .ok_or_else(|| "System git executable is not available on PATH".to_string())?;
    let output = std::process::Command::new(&git)
        .arg("clone")
        .arg("--")
        .arg(clone_url)
        .arg(clone_dir)
        .output()
        .map_err(|err| format!("Failed to launch `{}`: {err}", git.display()))?;

    if output.status.success() {
        return Ok(());
    }

    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let detail = match (stdout.is_empty(), stderr.is_empty()) {
        (true, true) => "git clone exited with no output".to_string(),
        (false, true) => stdout,
        (true, false) => stderr,
        (false, false) => format!("stderr: {stderr}; stdout: {stdout}"),
    };
    Err(format!("git clone failed: {detail}"))
}

fn checkout_git_ref(repo_dir: &Path, git_ref: &str) -> Result<(), ApiError> {
    let git_ref = git_ref.trim();
    if git_ref.is_empty() {
        return Ok(());
    }

    let repo = Repository::open(repo_dir)
        .map_err(|err| ApiError::BadRequest(format!("Failed to open cloned repository: {err}")))?;

    let rev_candidates = [
        git_ref.to_string(),
        format!("refs/heads/{git_ref}"),
        format!("refs/tags/{git_ref}"),
        format!("refs/remotes/origin/{git_ref}"),
    ];

    let mut last_err: Option<git2::Error> = None;

    for candidate in rev_candidates {
        match repo.revparse_ext(&candidate) {
            Ok((object, reference)) => {
                repo.checkout_tree(&object, None).map_err(|err| {
                    ApiError::BadRequest(format!("Failed to checkout git ref `{git_ref}`: {err}"))
                })?;

                if let Some(reference) = reference
                    && let Some(reference_name) = reference.name()
                {
                    repo.set_head(reference_name).map_err(|err| {
                        ApiError::BadRequest(format!(
                            "Failed to set HEAD for git ref `{git_ref}`: {err}"
                        ))
                    })?;
                    return Ok(());
                }

                repo.set_head_detached(object.id()).map_err(|err| {
                    ApiError::BadRequest(format!(
                        "Failed to detach HEAD for git ref `{git_ref}`: {err}"
                    ))
                })?;
                return Ok(());
            }
            Err(err) => {
                last_err = Some(err);
            }
        }
    }

    let detail = last_err
        .map(|err| err.to_string())
        .unwrap_or_else(|| "unknown git error".to_string());
    Err(ApiError::BadRequest(format!(
        "Could not resolve git ref `{git_ref}`: {detail}"
    )))
}

fn import_skills_from_repo_root(
    repo_root: &Path,
    canonical_dir: &Path,
    subpath: Option<&str>,
    skill_filter: Option<&str>,
    root_skill_folder_name: Option<&str>,
) -> Result<ImportSkillsResponse, ApiError> {
    let import_root = resolve_import_root(repo_root, subpath)?;
    let discovered = discover_importable_skills(&import_root, root_skill_folder_name)?;
    if discovered.is_empty() {
        return Err(ApiError::BadRequest(format!(
            "No skills found under `{}`",
            import_root.display()
        )));
    }

    let filtered = filter_skills(discovered, skill_filter);
    if filtered.is_empty() {
        return Err(ApiError::BadRequest(
            "No skills matched the provided skill_filter".to_string(),
        ));
    }

    let mut imported = Vec::new();
    let mut skipped = Vec::new();

    for skill in filtered {
        let destination = canonical_dir.join(&skill.folder_name);
        if path_entry_exists(&destination) {
            skipped.push(skill.folder_name.clone());
            continue;
        }

        copy_directory_recursive(&skill.path, &destination)?;
        let mut imported_info = skill.info.clone();
        imported_info.path = destination.to_string_lossy().to_string();
        imported.push(imported_info);
    }

    Ok(ImportSkillsResponse {
        imported,
        skipped,
        warnings: Vec::new(),
    })
}

fn resolve_import_root(repo_root: &Path, subpath: Option<&str>) -> Result<PathBuf, ApiError> {
    let Some(raw_subpath) = normalize_optional_string(subpath) else {
        return Ok(repo_root.to_path_buf());
    };

    let normalized_subpath = normalize_relative_path(&raw_subpath)?;
    let requested = repo_root.join(normalized_subpath);
    let canonical_repo_root = std::fs::canonicalize(repo_root)?;
    let canonical_requested = std::fs::canonicalize(&requested).map_err(|err| {
        ApiError::BadRequest(format!(
            "Import subpath `{}` could not be resolved: {err}",
            requested.display()
        ))
    })?;

    if !canonical_requested.starts_with(&canonical_repo_root) {
        return Err(ApiError::BadRequest(
            "Import subpath must stay within the cloned repository".to_string(),
        ));
    }
    if !canonical_requested.is_dir() {
        return Err(ApiError::BadRequest(format!(
            "Import subpath `{}` is not a directory",
            canonical_requested.display()
        )));
    }

    Ok(canonical_requested)
}

fn discover_importable_skills(
    root: &Path,
    root_skill_folder_name: Option<&str>,
) -> Result<Vec<CanonicalSkill>, ApiError> {
    let root_skill_md = root.join(SKILL_MD_FILE);
    if root_skill_md.is_file() {
        let folder_name = normalize_optional_string(root_skill_folder_name)
            .and_then(|name| normalize_skill_name(&name).ok())
            .or_else(|| {
                root.file_name()
                    .and_then(|name| name.to_str())
                    .and_then(|name| normalize_skill_name(name).ok())
            })
            .unwrap_or_else(|| "skill".to_string());
        let info = read_skill_info(&root_skill_md, &folder_name, root.to_path_buf())?;
        return Ok(vec![CanonicalSkill {
            folder_name,
            path: root.to_path_buf(),
            info,
        }]);
    }

    discover_skills_in_dir(root)
}

fn filter_skills(skills: Vec<CanonicalSkill>, skill_filter: Option<&str>) -> Vec<CanonicalSkill> {
    let Some(filter_value) = normalize_optional_string(skill_filter) else {
        return skills;
    };

    let needle = filter_value.to_lowercase();
    skills
        .into_iter()
        .filter(|skill| {
            skill.folder_name.to_lowercase().contains(&needle)
                || skill.info.name.to_lowercase().contains(&needle)
        })
        .collect()
}

fn discover_skills_in_dir(skills_dir: &Path) -> Result<Vec<CanonicalSkill>, ApiError> {
    let mut skills = Vec::new();
    for entry in std::fs::read_dir(skills_dir)? {
        let entry = entry?;
        let entry_path = entry.path();
        if !entry.file_type()?.is_dir() {
            continue;
        }

        let skill_md_path = entry_path.join(SKILL_MD_FILE);
        if !skill_md_path.is_file() {
            continue;
        }

        let folder_name = entry.file_name().to_string_lossy().to_string();
        let info = read_skill_info(&skill_md_path, &folder_name, entry_path.clone())?;
        skills.push(CanonicalSkill {
            folder_name,
            path: entry_path,
            info,
        });
    }

    skills.sort_by(|left, right| left.folder_name.cmp(&right.folder_name));
    Ok(skills)
}

fn read_skill_info(
    skill_md_path: &Path,
    fallback_name: &str,
    skill_path: PathBuf,
) -> Result<SkillInfo, ApiError> {
    let content = std::fs::read_to_string(skill_md_path)?;
    let metadata = parse_frontmatter_metadata(&content);

    let name = metadata
        .as_ref()
        .and_then(|map| map.get("name"))
        .map(|value| value.trim())
        .filter(|value| !value.is_empty())
        .unwrap_or(fallback_name)
        .to_string();

    let description = metadata
        .as_ref()
        .and_then(|map| map.get("description"))
        .map(|value| value.trim().to_string())
        .unwrap_or_default();

    Ok(SkillInfo {
        name,
        description,
        path: skill_path.to_string_lossy().to_string(),
    })
}

fn parse_frontmatter_metadata(content: &str) -> Option<HashMap<String, String>> {
    let lines = content.lines().collect::<Vec<_>>();
    if lines.first()?.trim() != "---" {
        return None;
    }

    let mut metadata = HashMap::new();
    let mut index = 1;
    while index < lines.len() {
        let line = lines[index];
        let trimmed = line.trim();
        if trimmed == "---" {
            return Some(metadata);
        }

        if trimmed.is_empty() || trimmed.starts_with('#') {
            index += 1;
            continue;
        }

        // Ignore nested YAML blocks (metadata: or indented values).
        if line.starts_with(' ') || line.starts_with('\t') {
            index += 1;
            continue;
        }

        let Some((key, raw_value)) = trimmed.split_once(':') else {
            index += 1;
            continue;
        };

        let key = key.trim();
        if key.is_empty() {
            index += 1;
            continue;
        }

        let raw_value = raw_value.trim();
        if let Some(block_style) = parse_block_style(raw_value) {
            let mut block_lines = Vec::new();
            let mut block_index = index + 1;
            while block_index < lines.len() {
                let block_line = lines[block_index];
                let block_trimmed = block_line.trim();
                if block_trimmed == "---" {
                    break;
                }
                if block_trimmed.is_empty()
                    || block_line.starts_with(' ')
                    || block_line.starts_with('\t')
                {
                    block_lines.push(block_line.to_string());
                    block_index += 1;
                    continue;
                }
                break;
            }

            metadata.insert(
                key.to_string(),
                parse_block_scalar_value(&block_lines, block_style),
            );
            index = block_index;
            continue;
        }

        metadata.insert(key.to_string(), strip_wrapping_quotes(raw_value));
        index += 1;
    }

    None
}

#[derive(Copy, Clone)]
enum YamlBlockStyle {
    Literal,
    Folded,
}

fn parse_block_style(raw_value: &str) -> Option<YamlBlockStyle> {
    let mut chars = raw_value.chars();
    let style = match chars.next()? {
        '|' => YamlBlockStyle::Literal,
        '>' => YamlBlockStyle::Folded,
        _ => return None,
    };

    for ch in chars {
        if ch == '+' || ch == '-' || ch.is_ascii_digit() {
            continue;
        }
        if ch.is_whitespace() {
            break;
        }
        return None;
    }

    Some(style)
}

fn parse_block_scalar_value(lines: &[String], style: YamlBlockStyle) -> String {
    let min_indent = lines
        .iter()
        .filter(|line| !line.trim().is_empty())
        .map(|line| {
            line.chars()
                .take_while(|ch| *ch == ' ' || *ch == '\t')
                .count()
        })
        .min()
        .unwrap_or(0);

    let normalized = lines
        .iter()
        .map(|line| {
            if line.trim().is_empty() {
                String::new()
            } else {
                line.chars().skip(min_indent).collect::<String>()
            }
        })
        .collect::<Vec<_>>();

    match style {
        YamlBlockStyle::Literal => normalized.join("\n"),
        YamlBlockStyle::Folded => fold_block_lines(&normalized),
    }
}

fn fold_block_lines(lines: &[String]) -> String {
    let mut folded = String::new();
    for (index, line) in lines.iter().enumerate() {
        if index > 0 {
            let previous_is_blank = lines[index - 1].trim().is_empty();
            let current_is_blank = line.trim().is_empty();
            if previous_is_blank || current_is_blank {
                folded.push('\n');
            } else {
                folded.push(' ');
            }
        }
        folded.push_str(line);
    }
    folded
}

fn strip_wrapping_quotes(value: &str) -> String {
    let bytes = value.as_bytes();
    if bytes.len() >= 2 {
        let first = bytes[0];
        let last = bytes[bytes.len() - 1];
        if (first == b'"' && last == b'"') || (first == b'\'' && last == b'\'') {
            return value[1..value.len() - 1].to_string();
        }
    }
    value.to_string()
}

fn build_links_response(
    executor: BaseCodingAgent,
    agent_dir: &Path,
    canonical_skills: &[CanonicalSkill],
) -> Result<GetSkillLinksResponse, ApiError> {
    let canonical_skill_names: HashSet<String> = canonical_skills
        .iter()
        .map(|skill| skill.folder_name.clone())
        .collect();

    let mut links = canonical_skills
        .iter()
        .map(|skill| {
            let agent_path = agent_dir.join(&skill.folder_name);
            let is_linked_to_canonical = is_matching_symlink(&agent_path, &skill.path)?;
            let uses_canonical_dir_directly = agent_path == skill.path;
            let is_legacy = !is_linked_to_canonical
                && !uses_canonical_dir_directly
                && is_legacy_skill_path(&agent_path)?;
            let state = if is_linked_to_canonical || uses_canonical_dir_directly || is_legacy {
                SkillLinkState::Linked
            } else {
                SkillLinkState::NotLinked
            };

            Ok(AgentSkillLinkInfo {
                skill_name: skill.folder_name.clone(),
                description: skill.info.description.clone(),
                state,
                agent_path: agent_path.to_string_lossy().to_string(),
                canonical_path: skill.path.to_string_lossy().to_string(),
                is_legacy,
            })
        })
        .collect::<Result<Vec<_>, ApiError>>()?;

    links.extend(discover_legacy_agent_skills(
        agent_dir,
        &canonical_skill_names,
    )?);
    links.sort_by(|left, right| left.skill_name.cmp(&right.skill_name));

    Ok(GetSkillLinksResponse {
        executor,
        agent_dir: agent_dir.to_string_lossy().to_string(),
        links,
    })
}

fn discover_legacy_agent_skills(
    agent_dir: &Path,
    canonical_skill_names: &HashSet<String>,
) -> Result<Vec<AgentSkillLinkInfo>, ApiError> {
    let mut legacy_skills = Vec::new();
    for entry in std::fs::read_dir(agent_dir)? {
        let entry = entry?;
        let folder_name = entry.file_name().to_string_lossy().to_string();
        if canonical_skill_names.contains(&folder_name) {
            continue;
        }

        let entry_path = entry.path();
        if !is_legacy_skill_path(&entry_path)? {
            continue;
        }

        let info = read_skill_info(
            &entry_path.join(SKILL_MD_FILE),
            &folder_name,
            entry_path.clone(),
        )?;
        let entry_path_str = entry_path.to_string_lossy().to_string();
        legacy_skills.push(AgentSkillLinkInfo {
            skill_name: folder_name,
            description: info.description,
            state: SkillLinkState::Linked,
            agent_path: entry_path_str.clone(),
            canonical_path: entry_path_str,
            is_legacy: true,
        });
    }

    legacy_skills.sort_by(|left, right| left.skill_name.cmp(&right.skill_name));
    Ok(legacy_skills)
}

fn is_legacy_skill_path(path: &Path) -> Result<bool, ApiError> {
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(err) => return Err(err.into()),
    };
    let file_type = metadata.file_type();
    if !(file_type.is_dir() || file_type.is_symlink()) {
        return Ok(false);
    }

    Ok(path.join(SKILL_MD_FILE).is_file())
}

fn normalize_skill_names(skill_names: Vec<String>) -> Result<Vec<String>, ApiError> {
    let mut deduped = BTreeSet::new();
    for skill_name in skill_names {
        deduped.insert(normalize_skill_name(&skill_name)?);
    }
    Ok(deduped.into_iter().collect())
}

fn normalize_skill_name(skill_name: &str) -> Result<String, ApiError> {
    let trimmed = skill_name.trim();
    if trimmed.is_empty() {
        return Err(ApiError::BadRequest(
            "Skill name must not be empty".to_string(),
        ));
    }

    let path = Path::new(trimmed);
    let mut components = path.components();
    let Some(first) = components.next() else {
        return Err(ApiError::BadRequest(
            "Skill name must be a single directory name".to_string(),
        ));
    };
    if components.next().is_some() || !matches!(first, Component::Normal(_)) {
        return Err(ApiError::BadRequest(format!(
            "Invalid skill name `{trimmed}`"
        )));
    }

    Ok(trimmed.to_string())
}

fn normalize_optional_string(value: Option<&str>) -> Option<String> {
    value
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
}

fn normalize_relative_path(path_value: &str) -> Result<String, ApiError> {
    let path = Path::new(path_value);
    if path.is_absolute() {
        return Err(ApiError::BadRequest(
            "Path must be relative to the repository root".to_string(),
        ));
    }

    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Normal(part) => normalized.push(part),
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                return Err(ApiError::BadRequest(
                    "Path must not traverse outside the repository root".to_string(),
                ));
            }
        }
    }

    if normalized.as_os_str().is_empty() {
        return Err(ApiError::BadRequest("Path must not be empty".to_string()));
    }

    Ok(normalized.to_string_lossy().to_string())
}

fn user_home_dir() -> Result<PathBuf, ApiError> {
    dirs::home_dir()
        .ok_or_else(|| ApiError::BadRequest("Could not determine the user home directory".into()))
}

fn canonical_skills_dir_for_home(home_dir: &Path) -> PathBuf {
    home_dir.join(".agents").join("skills")
}

fn ensure_canonical_skills_dir() -> Result<PathBuf, ApiError> {
    let home_dir = user_home_dir()?;
    ensure_canonical_skills_dir_for_home(&home_dir)
}

fn ensure_canonical_skills_dir_for_home(home_dir: &Path) -> Result<PathBuf, ApiError> {
    let canonical_dir = canonical_skills_dir_for_home(home_dir);
    std::fs::create_dir_all(&canonical_dir)?;
    Ok(canonical_dir)
}

fn ensure_agent_skills_dir(executor: BaseCodingAgent) -> Result<PathBuf, ApiError> {
    let home_dir = user_home_dir()?;
    let agent_dir = resolve_agent_skills_dir(executor, &home_dir)?;
    std::fs::create_dir_all(&agent_dir)?;
    Ok(agent_dir)
}

fn resolve_agent_skills_dir(
    executor: BaseCodingAgent,
    home_dir: &Path,
) -> Result<PathBuf, ApiError> {
    match executor {
        BaseCodingAgent::ClaudeCode => Ok(home_dir.join(".claude").join("skills")),
        BaseCodingAgent::Codex
        | BaseCodingAgent::Gemini
        | BaseCodingAgent::Opencode
        | BaseCodingAgent::Pi => Ok(canonical_skills_dir_for_home(home_dir)),
        _ => Err(ApiError::BadRequest(format!(
            "Executor `{executor}` is not supported for Skills manager"
        ))),
    }
}

fn unlink_skill_path(path: &Path) -> Result<(), ApiError> {
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(err) => return Err(err.into()),
    };

    if metadata.file_type().is_dir() && !metadata.file_type().is_symlink() {
        std::fs::remove_dir_all(path)?;
        return Ok(());
    }

    if let Err(file_err) = std::fs::remove_file(path) {
        if metadata.file_type().is_symlink() {
            std::fs::remove_dir(path).map_err(|_| file_err)?;
        } else {
            return Err(file_err.into());
        }
    }

    Ok(())
}

fn is_matching_symlink(link_path: &Path, expected_target: &Path) -> Result<bool, ApiError> {
    let metadata = match std::fs::symlink_metadata(link_path) {
        Ok(metadata) => metadata,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(err) => return Err(err.into()),
    };

    if !metadata.file_type().is_symlink() {
        return Ok(false);
    }

    let raw_target = std::fs::read_link(link_path)?;
    let resolved_target = if raw_target.is_absolute() {
        raw_target
    } else {
        link_path
            .parent()
            .unwrap_or_else(|| Path::new(""))
            .join(raw_target)
    };

    let canonical_link_target = match std::fs::canonicalize(&resolved_target) {
        Ok(path) => path,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(err) => return Err(err.into()),
    };
    let canonical_expected_target = std::fs::canonicalize(expected_target)?;

    Ok(canonical_link_target == canonical_expected_target)
}

fn path_entry_exists(path: &Path) -> bool {
    std::fs::symlink_metadata(path).is_ok()
}

fn copy_directory_recursive(source: &Path, destination: &Path) -> Result<(), ApiError> {
    std::fs::create_dir_all(destination)?;

    for entry in std::fs::read_dir(source)? {
        let entry = entry?;
        let entry_type = entry.file_type()?;
        let source_path = entry.path();
        let destination_path = destination.join(entry.file_name());

        if entry_type.is_dir() {
            copy_directory_recursive(&source_path, &destination_path)?;
        } else if entry_type.is_file() {
            std::fs::copy(&source_path, &destination_path)?;
        } else if entry_type.is_symlink() {
            let link_target = std::fs::read_link(&source_path)?;
            create_copy_symlink(&link_target, &destination_path, &source_path)?;
        }
    }

    Ok(())
}

#[cfg(unix)]
fn create_copy_symlink(
    target: &Path,
    link_path: &Path,
    _source_path: &Path,
) -> Result<(), ApiError> {
    std::os::unix::fs::symlink(target, link_path)?;
    Ok(())
}

#[cfg(windows)]
fn create_copy_symlink(
    target: &Path,
    link_path: &Path,
    source_path: &Path,
) -> Result<(), ApiError> {
    let points_to_dir = std::fs::metadata(source_path)
        .map(|metadata| metadata.is_dir())
        .unwrap_or(false);
    if points_to_dir {
        std::os::windows::fs::symlink_dir(target, link_path)?;
    } else {
        std::os::windows::fs::symlink_file(target, link_path)?;
    }
    Ok(())
}

#[cfg(unix)]
fn create_directory_symlink(target: &Path, link_path: &Path) -> Result<(), ApiError> {
    std::os::unix::fs::symlink(target, link_path)?;
    Ok(())
}

#[cfg(windows)]
fn create_directory_symlink(target: &Path, link_path: &Path) -> Result<(), ApiError> {
    std::os::windows::fs::symlink_dir(target, link_path)?;
    Ok(())
}

fn create_temp_import_dir() -> Result<PathBuf, ApiError> {
    let base = std::env::temp_dir().join("vibe-kanban-skill-import");
    std::fs::create_dir_all(&base)?;
    let dir = base.join(format!(
        "{}-{}",
        std::process::id(),
        Uuid::new_v4().simple()
    ));
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TestTempDir {
        path: PathBuf,
    }

    impl TestTempDir {
        fn new(prefix: &str) -> Self {
            let path = std::env::temp_dir().join(format!(
                "vibe-kanban-{prefix}-{}-{}",
                std::process::id(),
                Uuid::new_v4().simple()
            ));
            std::fs::create_dir_all(&path).expect("create test temp directory");
            Self { path }
        }

        fn path(&self) -> &Path {
            &self.path
        }
    }

    impl Drop for TestTempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }

    fn write_skill(dir: &Path, name: &str, description: &str) {
        std::fs::create_dir_all(dir).expect("create skill dir");
        let content = format!("---\nname: {name}\ndescription: {description}\n---\n");
        std::fs::write(dir.join(SKILL_MD_FILE), content).expect("write SKILL.md");
    }

    #[test]
    fn canonical_dir_resolution_points_to_shared_agents_skills() {
        let home_dir = PathBuf::from("/tmp/example-home");
        assert_eq!(
            canonical_skills_dir_for_home(&home_dir),
            home_dir.join(".agents").join("skills")
        );
    }

    #[test]
    fn agent_skills_dir_resolution_matches_executor_layout() {
        let home_dir = PathBuf::from("/tmp/example-home");
        assert_eq!(
            resolve_agent_skills_dir(BaseCodingAgent::ClaudeCode, &home_dir)
                .expect("resolve claude skills dir"),
            home_dir.join(".claude").join("skills")
        );
        assert_eq!(
            resolve_agent_skills_dir(BaseCodingAgent::Codex, &home_dir)
                .expect("resolve codex skills dir"),
            canonical_skills_dir_for_home(&home_dir)
        );
        assert_eq!(
            resolve_agent_skills_dir(BaseCodingAgent::Gemini, &home_dir)
                .expect("resolve gemini skills dir"),
            canonical_skills_dir_for_home(&home_dir)
        );
        assert_eq!(
            resolve_agent_skills_dir(BaseCodingAgent::Opencode, &home_dir)
                .expect("resolve opencode skills dir"),
            canonical_skills_dir_for_home(&home_dir)
        );
        assert_eq!(
            resolve_agent_skills_dir(BaseCodingAgent::Pi, &home_dir)
                .expect("resolve pi skills dir"),
            canonical_skills_dir_for_home(&home_dir)
        );
    }

    #[test]
    fn droid_skills_dir_resolution_is_rejected() {
        let home_dir = PathBuf::from("/tmp/example-home");
        let err = resolve_agent_skills_dir(BaseCodingAgent::Droid, &home_dir)
            .expect_err("droid should not be supported");
        assert!(
            err.to_string()
                .contains("is not supported for Skills manager")
        );
    }

    #[test]
    fn discover_skills_only_includes_directories_with_skill_md() {
        let temp = TestTempDir::new("discover-skills");
        write_skill(&temp.path().join("valid-skill"), "Valid", "Valid skill");
        std::fs::create_dir_all(temp.path().join("not-a-skill")).expect("create non-skill dir");
        std::fs::write(temp.path().join("plain-file.txt"), "hello").expect("create plain file");

        let discovered = discover_skills_in_dir(temp.path()).expect("discover skills");
        assert_eq!(discovered.len(), 1);
        assert_eq!(discovered[0].folder_name, "valid-skill");
        assert_eq!(discovered[0].info.name, "Valid");
    }

    #[test]
    fn parse_frontmatter_multiline_description_literal_block() {
        let content = r#"---
name: visualization-expert
description: |
  Chart selection and data visualization guidance for effective data communication.
  Use when: creating visualizations and choosing chart types.
license: MIT
metadata:
  author: awesome-llm-apps
  version: "1.0.0"
---
"#;

        let metadata =
            parse_frontmatter_metadata(content).expect("frontmatter metadata should parse");
        assert_eq!(
            metadata
                .get("description")
                .expect("description should exist"),
            "Chart selection and data visualization guidance for effective data communication.\nUse when: creating visualizations and choosing chart types."
        );
        assert_eq!(
            metadata.get("license").expect("license should parse"),
            "MIT"
        );
    }

    #[test]
    fn link_creation_is_valid_and_idempotent() {
        let temp = TestTempDir::new("link-idempotent");
        let canonical_skill = temp.path().join("canonical").join("skill-a");
        write_skill(&canonical_skill, "Skill A", "A");

        let agent_dir = temp.path().join("agent");
        std::fs::create_dir_all(&agent_dir).expect("create agent dir");
        let link_path = agent_dir.join("skill-a");

        create_directory_symlink(&canonical_skill, &link_path).expect("create first symlink");
        assert!(is_matching_symlink(&link_path, &canonical_skill).expect("check first symlink"));

        // Re-linking should be treated as a no-op by link validation logic.
        assert!(is_matching_symlink(&link_path, &canonical_skill).expect("check second symlink"));
    }

    #[test]
    fn link_conflict_when_non_link_path_exists() {
        let temp = TestTempDir::new("link-conflict");
        let canonical_skill = temp.path().join("canonical").join("skill-a");
        write_skill(&canonical_skill, "Skill A", "A");

        let agent_dir = temp.path().join("agent");
        let existing_path = agent_dir.join("skill-a");
        std::fs::create_dir_all(&existing_path).expect("create conflicting path");

        let is_conflict = path_entry_exists(&existing_path)
            && !is_matching_symlink(&existing_path, &canonical_skill).expect("check symlink");
        assert!(is_conflict);
    }

    #[test]
    fn unlink_only_removes_agent_path() {
        let temp = TestTempDir::new("unlink");
        let canonical_skill = temp.path().join("canonical").join("skill-a");
        write_skill(&canonical_skill, "Skill A", "A");

        let agent_dir = temp.path().join("agent");
        std::fs::create_dir_all(&agent_dir).expect("create agent dir");
        let link_path = agent_dir.join("skill-a");
        create_directory_symlink(&canonical_skill, &link_path).expect("create symlink");

        unlink_skill_path(&link_path).expect("unlink skill path");
        assert!(!path_entry_exists(&link_path));
        assert!(canonical_skill.exists());
    }

    #[test]
    fn links_response_marks_extra_agent_skill_as_legacy() {
        let temp = TestTempDir::new("legacy-extra");
        let canonical_dir = temp.path().join("canonical");
        write_skill(
            &canonical_dir.join("canonical-skill"),
            "Canonical Skill",
            "Canonical description",
        );
        let canonical_skills = discover_skills_in_dir(&canonical_dir).expect("discover skills");

        let legacy_source = temp.path().join("legacy-source").join("legacy-skill");
        write_skill(&legacy_source, "Legacy Skill", "Legacy description");

        let agent_dir = temp.path().join("agent");
        std::fs::create_dir_all(&agent_dir).expect("create agent dir");
        create_directory_symlink(&legacy_source, &agent_dir.join("legacy-skill"))
            .expect("create legacy symlink");

        let response =
            build_links_response(BaseCodingAgent::ClaudeCode, &agent_dir, &canonical_skills)
                .expect("build links response");
        let legacy_link = response
            .links
            .iter()
            .find(|link| link.skill_name == "legacy-skill")
            .expect("find legacy link");
        assert_eq!(legacy_link.state, SkillLinkState::Linked);
        assert!(legacy_link.is_legacy);
    }

    #[test]
    fn links_response_marks_mismatched_canonical_path_as_legacy() {
        let temp = TestTempDir::new("legacy-mismatch");
        let canonical_dir = temp.path().join("canonical");
        let canonical_skill = canonical_dir.join("skill-a");
        write_skill(&canonical_skill, "Skill A", "Canonical description");
        let canonical_skills = discover_skills_in_dir(&canonical_dir).expect("discover skills");

        let legacy_source = temp.path().join("legacy-source").join("skill-a");
        write_skill(&legacy_source, "Legacy Skill A", "Legacy description");

        let agent_dir = temp.path().join("agent");
        std::fs::create_dir_all(&agent_dir).expect("create agent dir");
        create_directory_symlink(&legacy_source, &agent_dir.join("skill-a"))
            .expect("create mismatched legacy symlink");

        let response =
            build_links_response(BaseCodingAgent::ClaudeCode, &agent_dir, &canonical_skills)
                .expect("build links response");
        let link = response
            .links
            .iter()
            .find(|entry| entry.skill_name == "skill-a")
            .expect("find canonical skill link");
        assert_eq!(link.state, SkillLinkState::Linked);
        assert!(link.is_legacy);
    }

    #[test]
    fn links_response_treats_shared_canonical_dir_as_linked() {
        let temp = TestTempDir::new("shared-canonical");
        let canonical_dir = temp.path().join("canonical");
        write_skill(
            &canonical_dir.join("skill-a"),
            "Skill A",
            "Canonical description",
        );
        let canonical_skills = discover_skills_in_dir(&canonical_dir).expect("discover skills");

        let response =
            build_links_response(BaseCodingAgent::Codex, &canonical_dir, &canonical_skills)
                .expect("build links response");
        let link = response
            .links
            .iter()
            .find(|entry| entry.skill_name == "skill-a")
            .expect("find canonical skill link");
        assert_eq!(link.state, SkillLinkState::Linked);
        assert!(!link.is_legacy);
    }

    #[test]
    fn source_parsing_supports_owner_repo_github_url_and_git_url() {
        let owner_repo =
            parse_import_source_values("openai/skills", None, None).expect("parse owner/repo");
        assert_eq!(owner_repo.clone_url, "https://github.com/openai/skills.git");
        assert_eq!(owner_repo.repo_name, "skills");
        assert_eq!(owner_repo.git_ref, None);

        let github_url = parse_import_source_values(
            "https://github.com/openai/skills/tree/main/skills/.curated",
            None,
            None,
        )
        .expect("parse github url");
        assert_eq!(github_url.clone_url, "https://github.com/openai/skills.git");
        assert_eq!(github_url.repo_name, "skills");
        assert_eq!(github_url.git_ref, Some("main".to_string()));
        assert_eq!(github_url.subpath, Some("skills/.curated".to_string()));

        let git_url =
            parse_import_source_values("https://gitlab.example.com/acme/skills.git", None, None)
                .expect("parse generic git url");
        assert_eq!(
            git_url.clone_url,
            "https://gitlab.example.com/acme/skills.git"
        );
        assert_eq!(git_url.repo_name, "skills");
    }

    #[test]
    fn import_without_subpath_imports_direct_skills() {
        let repo_root = TestTempDir::new("import-no-subpath");
        write_skill(
            &repo_root.path().join("alpha-skill"),
            "Alpha Skill",
            "Alpha description",
        );
        write_skill(
            &repo_root.path().join("beta-skill"),
            "Beta Skill",
            "Beta description",
        );
        std::fs::create_dir_all(repo_root.path().join("misc")).expect("create non-skill dir");

        let canonical_dir = TestTempDir::new("canonical-target");
        let result =
            import_skills_from_repo_root(repo_root.path(), canonical_dir.path(), None, None, None)
                .expect("import skills");

        assert_eq!(result.imported.len(), 2);
        assert!(result.skipped.is_empty());
        assert!(canonical_dir.path().join("alpha-skill").exists());
        assert!(canonical_dir.path().join("beta-skill").exists());
    }

    #[test]
    fn import_with_subpath_and_filter_selects_matching_skills() {
        let repo_root = TestTempDir::new("import-subpath-filter");
        let nested_root = repo_root.path().join("skills").join("community");
        write_skill(
            &nested_root.join("alpha-skill"),
            "Alpha Skill",
            "Alpha description",
        );
        write_skill(
            &nested_root.join("beta-skill"),
            "Beta Skill",
            "Beta description",
        );

        let canonical_dir = TestTempDir::new("canonical-filter-target");
        let result = import_skills_from_repo_root(
            repo_root.path(),
            canonical_dir.path(),
            Some("skills/community"),
            Some("alpha"),
            None,
        )
        .expect("import filtered skills");

        assert_eq!(result.imported.len(), 1);
        assert_eq!(result.imported[0].name, "Alpha Skill");
        assert!(canonical_dir.path().join("alpha-skill").exists());
        assert!(!canonical_dir.path().join("beta-skill").exists());
    }

    #[test]
    fn import_root_skill_uses_preferred_folder_name() {
        let repo_root = TestTempDir::new("import-root-skill");
        write_skill(repo_root.path(), "Root Skill", "Root description");

        let canonical_dir = TestTempDir::new("canonical-root-skill");
        let result = import_skills_from_repo_root(
            repo_root.path(),
            canonical_dir.path(),
            None,
            None,
            Some("project-skill"),
        )
        .expect("import root skill");

        assert_eq!(result.imported.len(), 1);
        assert!(canonical_dir.path().join("project-skill").exists());
    }

    #[test]
    fn preferred_root_skill_name_uses_filter_path_tail() {
        let source = ParsedImportSource {
            clone_url: "https://github.com/acme/project.git".to_string(),
            repo_name: "project".to_string(),
            git_ref: None,
            subpath: None,
        };
        let payload = ImportSkillsBody {
            source: source.clone_url.clone(),
            git_ref: None,
            subpath: None,
            skill_filter: Some("skill/xxx".to_string()),
        };

        assert_eq!(preferred_root_skill_folder_name(&source, &payload), "xxx");
    }

    #[test]
    fn git_cli_clone_supports_local_repositories() {
        let source_repo = TestTempDir::new("git-cli-clone-source");
        Repository::init(source_repo.path()).expect("initialize source repository");

        let destination_parent = TestTempDir::new("git-cli-clone-destination");
        let destination = destination_parent.path().join("repo");
        let source = source_repo.path().to_string_lossy().into_owned();

        clone_with_git_cli(&source, &destination).expect("clone with git cli");
        assert!(destination.join(".git").exists());
    }
}
