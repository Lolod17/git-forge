//! The in-memory domain model shared by the REST, GraphQL, MCP and smart-HTTP surfaces.
//!
//! Nothing in this module is persisted. Git objects are encoded by `git.rs`, and the logical clock
//! is deliberately deterministic so identical sequences of operations produce identical commits.

use crate::git::{self, Commit, ObjType, Signature, TreeEntry};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};
use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};

static LOGICAL_CLOCK: AtomicU64 = AtomicU64::new(0);
const BASE_UNIX_SECONDS: u64 = 1_794_873_600; // fixed logical epoch: 2026-11-17T00:00:00Z

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ObjectType {
    Commit,
    Tree,
    Blob,
    Tag,
}

impl ObjectType {
    fn git_type(self) -> ObjType {
        match self {
            Self::Commit => ObjType::Commit,
            Self::Tree => ObjType::Tree,
            Self::Blob => ObjType::Blob,
            Self::Tag => ObjType::Tag,
        }
    }

    fn from_git(kind: ObjType) -> Self {
        match kind {
            ObjType::Commit => Self::Commit,
            ObjType::Tree => Self::Tree,
            ObjType::Blob => Self::Blob,
            ObjType::Tag => Self::Tag,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitObject {
    pub object_type: ObjectType,
    pub data: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Label {
    pub id: u64,
    pub name: String,
    pub color: String,
    pub description: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Comment {
    pub id: u64,
    pub body: String,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Issue {
    pub id: u64,
    pub number: u64,
    pub title: String,
    pub body: String,
    pub state: String,
    pub state_reason: Option<String>,
    pub labels: Vec<String>,
    pub comments: Vec<Comment>,
    pub created_at: String,
    pub updated_at: String,
    pub closed_at: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pull {
    pub id: u64,
    pub number: u64,
    pub title: String,
    pub body: String,
    pub state: String,
    pub head: String,
    pub base: String,
    pub head_sha: String,
    pub base_sha: String,
    pub merged: bool,
    pub merged_at: Option<String>,
    pub merge_commit_sha: Option<String>,
    pub comments: Vec<Comment>,
    pub created_at: String,
    pub updated_at: String,
    pub closed_at: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileEntry {
    pub mode: String,
    pub oid: String,
}

#[derive(Debug, Clone)]
pub struct Repo {
    pub id: u64,
    pub name: String,
    pub description: Option<String>,
    pub is_private: bool,
    pub default_branch: String,
    pub has_issues: bool,
    pub created_at: String,
    pub updated_at: String,
    pub pushed_at: Option<String>,
    pub next_number: u64,
    pub objects: HashMap<String, GitObject>,
    pub refs: HashMap<String, String>,
    pub issues: BTreeMap<u64, Issue>,
    pub pulls: BTreeMap<u64, Pull>,
    pub labels: HashMap<String, Label>,
    pub deleted: bool,
    pub pack_cache: HashMap<String, Vec<u8>>,
    /// Packs already built for a fetch plan, shared by concurrent clones.
    pub served_packs: HashMap<String, std::sync::Arc<Vec<u8>>>,
}

impl Repo {
    pub fn new(id: u64, name: String) -> Self {
        let created_at = now_iso();
        Self {
            id,
            name,
            description: None,
            is_private: false,
            default_branch: "main".to_string(),
            has_issues: true,
            created_at: created_at.clone(),
            updated_at: created_at,
            pushed_at: None,
            next_number: 1,
            objects: HashMap::new(),
            refs: HashMap::new(),
            issues: BTreeMap::new(),
            pulls: BTreeMap::new(),
            labels: HashMap::new(),
            deleted: false,
            pack_cache: HashMap::new(),
            served_packs: HashMap::new(),
        }
    }

    pub fn get(&self, oid: &str) -> Option<&GitObject> {
        self.objects.get(oid)
    }

    pub fn branch_tip(&self, branch: &str) -> Option<&String> {
        self.refs.get(&format!("refs/heads/{branch}"))
    }

    pub fn entry_for(&mut self, oid: &str) -> Result<Vec<u8>, String> {
        if let Some(entry) = self.pack_cache.get(oid) {
            return Ok(entry.clone());
        }
        let object = self
            .objects
            .get(oid)
            .ok_or_else(|| format!("object {oid} is missing"))?;
        let entry = git::pack_entry(&git::GitObject {
            kind: object.object_type.git_type(),
            data: object.data.clone(),
        });
        self.pack_cache.insert(oid.to_string(), entry.clone());
        Ok(entry)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct User {
    pub login: String,
    pub id: u64,
    pub name: String,
    pub email: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValidationError {
    pub resource: String,
    pub code: String,
    pub field: Option<String>,
    pub message: Option<String>,
    pub value: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApiError {
    pub status: u16,
    pub message: String,
    pub errors: Vec<ValidationError>,
}

impl ApiError {
    pub fn new(status: u16, message: impl Into<String>) -> Self {
        Self { status, message: message.into(), errors: Vec::new() }
    }

    pub fn validation(
        status: u16,
        message: impl Into<String>,
        resource: &str,
        code: &str,
        field: Option<&str>,
    ) -> Self {
        Self {
            status,
            message: message.into(),
            errors: vec![ValidationError {
                resource: resource.to_string(),
                code: code.to_string(),
                field: field.map(str::to_string),
                message: None,
                value: None,
            }],
        }
    }
}

impl fmt::Display for ApiError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}", self.message)
    }
}

impl std::error::Error for ApiError {}

#[derive(Debug, Clone, Default)]
pub struct RepoUpdate {
    pub name: Option<String>,
    pub description: Option<Option<String>>,
    pub private: Option<bool>,
    pub visibility: Option<String>,
    pub has_issues: Option<bool>,
    pub default_branch: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct IssueUpdate {
    pub title: Option<String>,
    pub body: Option<String>,
    pub labels: Option<Vec<String>>,
    pub state: Option<String>,
    pub state_reason: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct PullUpdate {
    pub title: Option<String>,
    pub body: Option<String>,
    pub base: Option<String>,
    pub state: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct IssueFilter {
    pub state: Option<String>,
    pub sort: Option<String>,
    pub direction: Option<String>,
    pub labels: Vec<String>,
}

#[derive(Debug, Clone, Default)]
pub struct MergeOptions {
    pub method: Option<String>,
    pub title: Option<String>,
    pub message: Option<String>,
    pub sha: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileDiff {
    pub path: String,
    pub additions: usize,
    pub deletions: usize,
    pub status: String,
    pub sha: String,
    pub patch: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PathEntry {
    File { mode: String, oid: String },
    Directory { oid: String, entries: Vec<TreeEntry> },
}

#[derive(Debug)]
pub struct Forge {
    pub login: String,
    pub token: Option<String>,
    pub user: User,
    /// Lower-cased name index. Values are clones to keep both public indexes convenient to inspect;
    /// every store mutation synchronizes these snapshots.
    pub repos: HashMap<String, Repo>,
    pub repos_by_id: BTreeMap<u64, Repo>,
    pub next_repo_id: u64,
    pub next_issue_id: u64,
    pub next_comment_id: u64,
    pub next_label_id: u64,
    pub next_pull_id: u64,
    mergeable_cache: HashMap<(u64, String, String), bool>,
}

impl Forge {
    pub fn new(login: String, token: Option<String>) -> Self {
        let user = User {
            login: login.clone(),
            id: 1,
            name: login.clone(),
            email: format!("{login}@users.noreply.gitforge.local"),
        };
        Self {
            login,
            token,
            user,
            repos: HashMap::new(),
            repos_by_id: BTreeMap::new(),
            next_repo_id: 1001,
            next_issue_id: 1,
            next_comment_id: 1,
            next_label_id: 1,
            next_pull_id: 1,
            mergeable_cache: HashMap::new(),
        }
    }

    pub fn session_user(&self) -> &User { &self.user }

    fn sync_repo(&mut self, repo: &Repo) {
        if let Some(previous) = self.repos_by_id.get(&repo.id) {
            let old_key = previous.name.to_lowercase();
            if self.repos.get(&old_key).is_some_and(|entry| entry.id == repo.id) {
                self.repos.remove(&old_key);
            }
        }
        if repo.deleted {
            self.repos_by_id.remove(&repo.id);
        } else {
            self.repos_by_id.insert(repo.id, repo.clone());
            self.repos.insert(repo.name.to_lowercase(), repo.clone());
        }
    }

    pub fn is_viewer(&self, owner: &str) -> bool {
        owner.eq_ignore_ascii_case(&self.user.login)
    }

    pub fn list_repos(&self) -> Vec<Repo> {
        self.repos_by_id.values().filter(|repo| !repo.deleted).cloned().collect()
    }

    pub fn repo_by_name(&self, owner: &str, name: &str) -> Option<Repo> {
        if !self.is_viewer(owner) { return None; }
        self.repos.get(&name.to_lowercase()).filter(|repo| !repo.deleted).cloned()
    }

    pub fn repo_by_id(&self, id: u64) -> Option<Repo> {
        self.repos_by_id.get(&id).filter(|repo| !repo.deleted).cloned()
    }

    pub fn create_repo(
        &mut self,
        name: impl AsRef<str>,
        description: Option<String>,
        private: bool,
        has_issues: bool,
    ) -> Result<Repo, ApiError> {
        let name = name.as_ref().trim();
        if !valid_new_repo_name(name) {
            let mut error = ApiError::validation(422, "Repository creation failed.", "Repository", "custom", Some("name"));
            if let Some(detail) = error.errors.first_mut() { detail.message = Some("name is invalid".to_string()); }
            return Err(error);
        }
        let key = name.to_lowercase();
        if self.repos.contains_key(&key) {
            let mut error = ApiError::validation(422, "Repository creation failed.", "Repository", "custom", Some("name"));
            if let Some(detail) = error.errors.first_mut() { detail.message = Some("name already exists on this account".to_string()); }
            return Err(error);
        }
        let id = self.next_repo_id;
        self.next_repo_id = self.next_repo_id.saturating_add(1);
        let mut repo = Repo::new(id, name.to_string());
        repo.description = description;
        repo.is_private = private;
        repo.has_issues = has_issues;
        self.sync_repo(&repo);
        Ok(repo)
    }

    pub fn update_repo(&mut self, repo: &mut Repo, patch: RepoUpdate) -> Result<Repo, ApiError> {
        if let Some(branch) = patch.default_branch.as_deref() {
            if !repo.refs.is_empty() && repo.branch_tip(branch).is_none() {
                return Err(ApiError::validation(422, "Validation Failed", "Repository", "invalid", Some("default_branch")));
            }
        }
        if let Some(name) = patch.name.as_deref() {
            let name = name.trim();
            if !valid_repo_name(name) {
                return Err(ApiError::validation(422, "Validation Failed", "Repository", "invalid", Some("name")));
            }
            if self.repos.get(&name.to_lowercase()).is_some_and(|other| other.id != repo.id) {
                let mut error = ApiError::validation(422, "Validation Failed", "Repository", "custom", Some("name"));
                if let Some(detail) = error.errors.first_mut() { detail.message = Some("name already exists on this account".to_string()); }
                return Err(error);
            }
            repo.name = name.to_string();
        }
        if let Some(description) = patch.description { repo.description = description; }
        if let Some(value) = patch.private { repo.is_private = value; }
        if let Some(visibility) = patch.visibility.as_deref() {
            if visibility == "private" || visibility == "public" { repo.is_private = visibility == "private"; }
        }
        if let Some(value) = patch.has_issues { repo.has_issues = value; }
        if let Some(branch) = patch.default_branch {
            repo.default_branch = branch;
        }
        repo.updated_at = now_iso();
        self.sync_repo(repo);
        Ok(repo.clone())
    }

    pub fn delete_repo(&mut self, repo: &mut Repo) {
        repo.deleted = true;
        self.repos.remove(&repo.name.to_lowercase());
        self.repos_by_id.remove(&repo.id);
    }

    pub fn head_ref(&self, repo: &Repo) -> String { format!("refs/heads/{}", repo.default_branch) }

    pub fn peel(&self, repo: &Repo, oid: &str) -> String {
        let mut current = oid.to_string();
        for _ in 0..10 {
            match repo.get(&current) {
                Some(object) if object.object_type == ObjectType::Tag => {
                    let (target, _) = git::tag_target(&object.data);
                    current = target;
                }
                _ => break,
            }
        }
        current
    }

    pub fn branch_tip(&self, repo: &Repo, branch: &str) -> Option<String> {
        repo.branch_tip(branch).cloned()
    }

    pub fn write_ref(&mut self, repo: &mut Repo, reference: &str, oid: &str) {
        repo.refs.insert(reference.to_string(), oid.to_string());
        repo.pushed_at = Some(now_iso());
        self.sync_repo(repo);
    }

    pub fn delete_ref(&mut self, repo: &mut Repo, reference: &str) {
        repo.refs.remove(reference);
        repo.pushed_at = Some(now_iso());
        self.sync_repo(repo);
    }

    pub fn put_object(&mut self, repo: &mut Repo, object: GitObject) -> String {
        let oid = git::hash_object(object.object_type.git_type(), &object.data);
        if !repo.objects.contains_key(&oid) {
            repo.objects.insert(oid.clone(), object);
        }
        self.sync_repo(repo);
        oid
    }

    pub fn put_object_data(&mut self, repo: &mut Repo, object_type: ObjectType, data: Vec<u8>) -> String {
        self.put_object(repo, GitObject { object_type, data })
    }

    pub fn put_objects(&mut self, repo: &mut Repo, objects: &HashMap<String, git::GitObject>) {
        for (oid, object) in objects {
            repo.objects.entry(oid.clone()).or_insert_with(|| GitObject {
                object_type: ObjectType::from_git(object.kind),
                data: object.data.clone(),
            });
        }
        self.sync_repo(repo);
    }

    pub fn object<'a>(&self, repo: &'a Repo, oid: &str) -> Option<&'a GitObject> { repo.get(oid) }

    pub fn entry_for(&self, repo: &mut Repo, oid: &str) -> Result<Vec<u8>, String> {
        repo.entry_for(oid)
    }

    /// Parse a received pack, store its objects, and return newly received commit tips (commits which
    /// are not parents of another commit from this pack), in deterministic OID order.
    pub fn ingest_pack(&mut self, repo: &mut Repo, bytes: &[u8]) -> Result<Vec<String>, String> {
        let objects = git::read_pack(bytes, &|oid| repo.get(oid).map(|object| git::GitObject {
            kind: object.object_type.git_type(), data: object.data.clone(),
        }))?;
        let mut parents = HashSet::new();
        let mut commits = Vec::new();
        for (oid, object) in &objects {
            if object.kind == ObjType::Commit {
                let commit = git::parse_commit(&object.data);
                parents.extend(commit.parents);
                commits.push(oid.clone());
            }
        }
        commits.retain(|oid| !parents.contains(oid));
        commits.sort();
        self.put_objects(repo, &objects);
        Ok(commits)
    }

    pub fn commit_of(&self, repo: &Repo, oid: &str) -> Option<Commit> {
        let object = repo.get(oid)?;
        (object.object_type == ObjectType::Commit).then(|| git::parse_commit(&object.data))
    }

    pub fn resolve(&self, repo: &Repo, refish: &str) -> Option<String> {
        if refish.is_empty() { return None; }
        let refs: Vec<String> = if refish.starts_with("refs/") {
            vec![refish.to_string()]
        } else {
            vec![format!("refs/heads/{refish}"), format!("refs/tags/{refish}")]
        };
        for name in refs {
            if let Some(oid) = repo.refs.get(&name) { return Some(self.peel(repo, oid)); }
        }
        if git::is_oid(refish) && repo.get(refish).is_some_and(|object| object.object_type == ObjectType::Commit) {
            return Some(refish.to_string());
        }
        if (7..40).contains(&refish.len()) && refish.bytes().all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)) {
            return repo.objects.iter().find_map(|(oid, object)| {
                (object.object_type == ObjectType::Commit && oid.starts_with(refish)).then(|| oid.clone())
            });
        }
        None
    }

    pub fn is_ancestor(&self, repo: &Repo, ancestor: &str, descendant: &str) -> bool {
        if ancestor == descendant { return true; }
        let mut seen = HashSet::new();
        let mut stack = vec![descendant.to_string()];
        while let Some(oid) = stack.pop() {
            if oid == ancestor { return true; }
            if !seen.insert(oid.clone()) { continue; }
            if let Some(commit) = self.commit_of(repo, &oid) {
                stack.extend(commit.parents.into_iter().filter(|parent| !seen.contains(parent)));
            }
        }
        false
    }

    pub fn ancestors(&self, repo: &Repo, tip: &str) -> HashSet<String> {
        let mut seen = HashSet::new();
        let mut stack = vec![tip.to_string()];
        while let Some(oid) = stack.pop() {
            if seen.contains(&oid) { continue; }
            let Some(commit) = self.commit_of(repo, &oid) else { continue; };
            seen.insert(oid);
            stack.extend(commit.parents.into_iter().filter(|parent| !seen.contains(parent)));
        }
        seen
    }

    pub fn merge_base(&self, repo: &Repo, a: &str, b: &str) -> Option<String> {
        let from_a = self.ancestors(repo, a);
        let mut seen = HashSet::new();
        let mut queue = VecDeque::from([b.to_string()]);
        while let Some(oid) = queue.pop_front() {
            if !seen.insert(oid.clone()) { continue; }
            if from_a.contains(&oid) { return Some(oid); }
            if let Some(commit) = self.commit_of(repo, &oid) { queue.extend(commit.parents); }
        }
        None
    }

    pub fn log(&self, repo: &Repo, tip: &str, stop: Option<&str>, skip: usize, limit: Option<usize>) -> Vec<String> {
        let excluded = stop.map(|oid| self.ancestors(repo, oid)).unwrap_or_default();
        let mut order = HashMap::new();
        let mut times = HashMap::new();
        let mut queue = VecDeque::from([tip.to_string()]);
        let mut visited = HashSet::new();
        while let Some(oid) = queue.pop_front() {
            if !visited.insert(oid.clone()) { continue; }
            order.insert(oid.clone(), order.len());
            if let Some(commit) = self.commit_of(repo, &oid) {
                times.insert(oid, committer_seconds(&commit.committer));
                queue.extend(commit.parents);
            }
        }
        let mut result: Vec<String> = self.ancestors(repo, tip).into_iter().filter(|oid| !excluded.contains(oid)).collect();
        result.sort_by(|a, b| times.get(b).copied().unwrap_or(0).cmp(&times.get(a).copied().unwrap_or(0))
            .then_with(|| order.get(a).copied().unwrap_or(usize::MAX).cmp(&order.get(b).copied().unwrap_or(usize::MAX))));
        result.into_iter().skip(skip).take(limit.unwrap_or(usize::MAX)).collect()
    }

    pub fn tree_entries(&self, repo: &Repo, oid: &str) -> Result<Vec<TreeEntry>, String> {
        let Some(object) = repo.get(oid) else { return Err(format!("tree {oid} is missing")); };
        if object.object_type != ObjectType::Tree { return Err(format!("tree {oid} is missing")); }
        git::parse_tree(&object.data)
    }

    pub fn tree_of_commit(&self, repo: &Repo, oid: &str) -> Result<Vec<TreeEntry>, String> {
        let Some(commit) = self.commit_of(repo, oid) else { return Ok(Vec::new()); };
        self.tree_entries(repo, &commit.tree)
    }

    fn flat_of_commit(&self, repo: &Repo, oid: Option<&str>) -> Result<BTreeMap<String, FileEntry>, String> {
        let Some(oid) = oid else { return Ok(BTreeMap::new()); };
        let Some(commit) = self.commit_of(repo, oid) else { return Ok(BTreeMap::new()); };
        self.flatten_tree(repo, &commit.tree)
    }

    pub fn flatten_tree(&self, repo: &Repo, tree_oid: &str) -> Result<BTreeMap<String, FileEntry>, String> {
        let mut files = BTreeMap::new();
        self.flatten_into(repo, tree_oid, "", &mut files)?;
        Ok(files)
    }

    fn flatten_into(&self, repo: &Repo, tree_oid: &str, prefix: &str, out: &mut BTreeMap<String, FileEntry>) -> Result<(), String> {
        for entry in self.tree_entries(repo, tree_oid)? {
            let path = if prefix.is_empty() { entry.name.clone() } else { format!("{prefix}/{}", entry.name) };
            if entry.mode == "40000" { self.flatten_into(repo, &entry.oid, &path, out)?; }
            else { out.insert(path, FileEntry { mode: entry.mode, oid: entry.oid }); }
        }
        Ok(())
    }

    pub fn build_tree(&mut self, repo: &mut Repo, files: &BTreeMap<String, FileEntry>) -> Result<String, ApiError> {
        #[derive(Default)]
        struct Node { files: BTreeMap<String, FileEntry>, dirs: BTreeMap<String, Node> }
        fn insert(root: &mut Node, path: &str, entry: &FileEntry) -> Result<(), ApiError> {
            let segments: Vec<&str> = path.split('/').collect();
            if segments.is_empty() || segments.iter().any(|segment| segment.is_empty()) {
                return Err(ApiError::new(409, "invalid empty path segment"));
            }
            let mut node = root;
            for (index, segment) in segments.iter().take(segments.len().saturating_sub(1)).enumerate() {
                if node.files.contains_key(*segment) {
                    return Err(ApiError::new(409, format!("{} is a file, not a directory", segments[..=index].join("/"))));
                }
                node = node.dirs.entry((*segment).to_string()).or_default();
            }
            let leaf = segments[segments.len() - 1];
            if node.dirs.contains_key(leaf) { return Err(ApiError::new(409, format!("{path} is a directory, not a file"))); }
            node.files.insert(leaf.to_string(), entry.clone());
            Ok(())
        }
        fn write_node(forge: &mut Forge, repo: &mut Repo, node: &Node) -> String {
            let mut entries = Vec::with_capacity(node.files.len() + node.dirs.len());
            for (name, file) in &node.files {
                entries.push(TreeEntry { mode: file.mode.clone(), name: name.clone(), oid: file.oid.clone() });
            }
            for (name, child) in &node.dirs {
                let oid = write_node(forge, repo, child);
                entries.push(TreeEntry { mode: "40000".to_string(), name: name.clone(), oid });
            }
            let data = git::encode_tree(&entries);
            forge.put_object_data(repo, ObjectType::Tree, data)
        }
        let mut root = Node::default();
        for (path, entry) in files { insert(&mut root, path, entry)?; }
        Ok(write_node(self, repo, &root))
    }

    pub fn lookup_path(&self, repo: &Repo, commit_oid: &str, path: &str) -> Result<Option<PathEntry>, String> {
        let Some(commit) = self.commit_of(repo, commit_oid) else { return Ok(None); };
        let segments: Vec<&str> = path.split('/').filter(|segment| !segment.is_empty()).collect();
        let mut tree_oid = commit.tree;
        if segments.is_empty() {
            return Ok(Some(PathEntry::Directory { oid: tree_oid.clone(), entries: self.tree_entries(repo, &tree_oid)? }));
        }
        for (index, segment) in segments.iter().enumerate() {
            let Some(entry) = self.tree_entries(repo, &tree_oid)?.into_iter().find(|entry| entry.name == *segment) else { return Ok(None); };
            if index + 1 == segments.len() {
                if entry.mode == "40000" {
                    return Ok(Some(PathEntry::Directory { oid: entry.oid.clone(), entries: self.tree_entries(repo, &entry.oid)? }));
                }
                return Ok(Some(PathEntry::File { mode: entry.mode, oid: entry.oid }));
            }
            if entry.mode != "40000" { return Ok(None); }
            tree_oid = entry.oid;
        }
        Ok(None)
    }

    pub fn commit_tree(&mut self, repo: &mut Repo, parents: &[String], tree: &str, message: &str) -> String {
        self.make_commit(repo, tree, parents, message, None)
    }

    fn make_commit(&mut self, repo: &mut Repo, tree: &str, parents: &[String], message: &str, author: Option<Signature>) -> String {
        let time = next_unix_seconds() as i64;
        let committer = Signature { name: self.user.name.clone(), email: self.user.email.clone(), time, tz: "+0000".to_string() };
        let author = author.unwrap_or_else(|| committer.clone());
        let data = git::encode_commit(tree, parents, &author, &committer, message);
        self.put_object_data(repo, ObjectType::Commit, data)
    }

    pub fn contents_commit(
        &mut self,
        repo: &mut Repo,
        path: &str,
        content: &[u8],
        message: &str,
        branch: Option<&str>,
        expected_sha: Option<&str>,
    ) -> Result<(String, String), ApiError> {
        let branch = branch.map(str::to_string).unwrap_or_else(|| repo.default_branch.clone());
        let path = normalize_path(path)?;
        let tip = repo.branch_tip(&branch).cloned();
        if tip.is_none() && !repo.refs.is_empty() { return Err(ApiError::new(404, format!("Branch {branch} not found"))); }
        let mut files = match tip.as_deref() {
            Some(oid) => {
                let commit = self.commit_of(repo, oid).ok_or_else(|| ApiError::new(500, format!("commit {oid} is missing")))?;
                self.flatten_tree(repo, &commit.tree).map_err(|error| ApiError::new(500, error))?
            }
            None => BTreeMap::new(),
        };
        let existing = files.get(&path).cloned();
        if let Some(file) = &existing {
            if expected_sha.is_none_or(str::is_empty) { return Err(ApiError::new(422, "Invalid request.\n\n\"sha\" wasn't supplied.")); }
            if expected_sha != Some(file.oid.as_str()) { return Err(ApiError::new(409, format!("{path} does not match {}", expected_sha.unwrap_or_default()))); }
        }
        let blob = self.put_object_data(repo, ObjectType::Blob, content.to_vec());
        files.insert(path, FileEntry { mode: existing.map(|file| file.mode).unwrap_or_else(|| "100644".to_string()), oid: blob.clone() });
        let tree = self.build_tree(repo, &files)?;
        let parents: Vec<String> = tip.iter().cloned().collect();
        let commit = self.commit_tree(repo, &parents, &tree, message);
        self.write_ref(repo, &format!("refs/heads/{branch}"), &commit);
        if tip.is_none() { self.adopt_default_branch(repo, &[branch]); }
        Ok((commit, blob))
    }

    pub fn delete_contents(
        &mut self,
        repo: &mut Repo,
        path: &str,
        message: &str,
        branch: Option<&str>,
        sha: Option<&str>,
    ) -> Result<String, ApiError> {
        let branch = branch.map(str::to_string).unwrap_or_else(|| repo.default_branch.clone());
        let path = normalize_path(path)?;
        let tip = repo.branch_tip(&branch).cloned().ok_or_else(|| ApiError::new(404, "Not Found"))?;
        let commit_data = self.commit_of(repo, &tip).ok_or_else(|| ApiError::new(500, format!("commit {tip} is missing")))?;
        let mut files = self.flatten_tree(repo, &commit_data.tree).map_err(|error| ApiError::new(500, error))?;
        let existing = files.get(&path).ok_or_else(|| ApiError::new(404, "Not Found"))?;
        if sha.is_none_or(str::is_empty) { return Err(ApiError::new(422, "Invalid request.\n\n\"sha\" wasn't supplied.")); }
        if sha != Some(existing.oid.as_str()) { return Err(ApiError::new(409, format!("{path} does not match {}", sha.unwrap_or_default()))); }
        files.remove(&path);
        let tree = self.build_tree(repo, &files)?;
        let commit = self.commit_tree(repo, &[tip], &tree, message);
        self.write_ref(repo, &format!("refs/heads/{branch}"), &commit);
        Ok(commit)
    }

    pub fn create_label(&mut self, repo: &mut Repo, name: &str, color: Option<&str>, description: Option<String>) -> Result<Label, ApiError> {
        let name = name.trim();
        if name.is_empty() { return Err(ApiError::validation(422, "Validation Failed", "Label", "missing_field", Some("name"))); }
        if repo.labels.contains_key(&name.to_lowercase()) { return Err(ApiError::validation(422, "Validation Failed", "Label", "already_exists", Some("name"))); }
        let color = color.filter(|value| valid_hex_color(value)).map(|value| value.trim_start_matches('#').to_lowercase()).unwrap_or_else(|| "ededed".to_string());
        let label = Label { id: self.next_label_id, name: name.to_string(), color, description };
        self.next_label_id = self.next_label_id.saturating_add(1);
        repo.labels.insert(name.to_lowercase(), label.clone());
        self.sync_repo(repo);
        Ok(label)
    }

    pub fn get_label(&self, repo: &Repo, name: &str) -> Option<Label> { repo.labels.get(&name.to_lowercase()).cloned() }

    pub fn list_labels(&self, repo: &Repo) -> Vec<Label> {
        let mut labels: Vec<Label> = repo.labels.values().cloned().collect();
        labels.sort_by_key(|label| label.id);
        labels
    }

    pub fn label_names(&self, repo: &Repo, names: &[String]) -> Result<Vec<String>, ApiError> {
        let mut result = Vec::new();
        let mut seen = HashSet::new();
        for name in names {
            let Some(label) = repo.labels.get(&name.to_lowercase()) else {
                let mut error = ApiError::validation(422, format!("Validation Failed: the label {} does not exist in this repository", json_quote(name)), "Label", "invalid", Some("labels"));
                if let Some(detail) = error.errors.first_mut() { detail.value = Some(name.clone()); }
                return Err(error);
            };
            if seen.insert(label.name.clone()) { result.push(label.name.clone()); }
        }
        Ok(result)
    }

    pub fn create_issue(&mut self, repo: &mut Repo, title: &str, body: &str, labels: &[String]) -> Result<Issue, ApiError> {
        if title.trim().is_empty() { return Err(ApiError::validation(422, "Validation Failed", "Issue", "missing_field", Some("title"))); }
        let labels = self.label_names(repo, labels)?;
        let now = now_iso();
        let issue = Issue {
            id: self.next_issue_id,
            number: repo.next_number,
            title: title.to_string(), body: body.to_string(), state: "open".to_string(), state_reason: None,
            labels, comments: Vec::new(), created_at: now.clone(), updated_at: now, closed_at: None,
        };
        self.next_issue_id = self.next_issue_id.saturating_add(1);
        repo.next_number = repo.next_number.saturating_add(1);
        repo.issues.insert(issue.number, issue.clone());
        self.sync_repo(repo);
        Ok(issue)
    }

    pub fn get_issue(&self, repo: &Repo, number: u64) -> Option<Issue> { repo.issues.get(&number).cloned() }

    pub fn update_issue(&mut self, repo: &mut Repo, number: u64, patch: IssueUpdate) -> Result<Issue, ApiError> {
        let mut issue = repo.issues.get(&number).cloned().ok_or_else(|| ApiError::new(404, "Not Found"))?;
        if let Some(title) = patch.title {
            if title.trim().is_empty() { return Err(ApiError::validation(422, "Validation Failed", "Issue", "invalid", Some("title"))); }
            issue.title = title;
        }
        if let Some(body) = patch.body { issue.body = body; }
        if let Some(labels) = patch.labels { issue.labels = self.label_names(repo, &labels)?; }
        if let Some(state) = patch.state {
            if state != "open" && state != "closed" { return Err(ApiError::validation(422, "Validation Failed", "Issue", "invalid", Some("state"))); }
            if issue.state != state {
                issue.state = state.clone();
                issue.closed_at = if state == "closed" { Some(now_iso()) } else { None };
                issue.state_reason = if state == "closed" { Some(patch.state_reason.unwrap_or_else(|| "completed".to_string())) } else { Some("reopened".to_string()) };
            }
        }
        issue.updated_at = now_iso();
        repo.issues.insert(number, issue.clone());
        self.sync_repo(repo);
        Ok(issue)
    }

    pub fn list_issues(&self, repo: &Repo, filter: &IssueFilter) -> Vec<Issue> {
        let state = filter.state.as_deref().unwrap_or("open");
        let wanted: Vec<String> = filter.labels.iter().map(|name| name.to_lowercase()).collect();
        let mut issues: Vec<Issue> = repo.issues.values().filter(|issue| {
            (state != "open" && state != "closed" || issue.state == state)
                && wanted.iter().all(|name| issue.labels.iter().any(|label| label.eq_ignore_ascii_case(name)))
        }).cloned().collect();
        let ascending = filter.direction.as_deref() == Some("asc");
        let sort = filter.sort.as_deref().unwrap_or("created");
        issues.sort_by(|a, b| {
            let ordering = match sort {
                "updated" => a.updated_at.cmp(&b.updated_at),
                "comments" => a.comments.len().cmp(&b.comments.len()),
                _ => a.created_at.cmp(&b.created_at),
            }.then_with(|| a.number.cmp(&b.number));
            if ascending { ordering } else { ordering.reverse() }
        });
        issues
    }

    pub fn add_comment(&mut self, repo: &mut Repo, number: u64, body: &str) -> Result<Comment, ApiError> {
        if let Some(issue) = repo.issues.get(&number) { return self.add_issue_comment(repo, number, body, issue.id); }
        if repo.pulls.contains_key(&number) { return self.add_pull_comment(repo, number, body); }
        Err(ApiError::new(404, "Not Found"))
    }

    fn add_issue_comment(&mut self, repo: &mut Repo, number: u64, body: &str, _issue_id: u64) -> Result<Comment, ApiError> {
        if body.is_empty() { return Err(ApiError::validation(422, "Validation Failed", "IssueComment", "missing_field", Some("body"))); }
        let now = now_iso();
        let comment = Comment { id: self.next_comment_id, body: body.to_string(), created_at: now.clone(), updated_at: now.clone() };
        self.next_comment_id = self.next_comment_id.saturating_add(1);
        if let Some(issue) = repo.issues.get_mut(&number) { issue.comments.push(comment.clone()); issue.updated_at = now; }
        self.sync_repo(repo);
        Ok(comment)
    }

    pub fn add_pull_comment(&mut self, repo: &mut Repo, number: u64, body: &str) -> Result<Comment, ApiError> {
        if body.is_empty() { return Err(ApiError::validation(422, "Validation Failed", "IssueComment", "missing_field", Some("body"))); }
        if !repo.pulls.contains_key(&number) { return Err(ApiError::new(404, "Not Found")); }
        let now = now_iso();
        let comment = Comment { id: self.next_comment_id, body: body.to_string(), created_at: now.clone(), updated_at: now.clone() };
        self.next_comment_id = self.next_comment_id.saturating_add(1);
        if let Some(pull) = repo.pulls.get_mut(&number) { pull.comments.push(comment.clone()); pull.updated_at = now; }
        self.sync_repo(repo);
        Ok(comment)
    }

    pub fn list_comments(&self, repo: &Repo, number: u64) -> Vec<Comment> {
        repo.issues.get(&number).map(|issue| issue.comments.clone())
            .or_else(|| repo.pulls.get(&number).map(|pull| pull.comments.clone())).unwrap_or_default()
    }

    pub fn create_pull(&mut self, repo: &mut Repo, title: &str, body: &str, head: &str, base: &str) -> Result<Pull, ApiError> {
        let head = head.split_once(':').map(|(_, branch)| branch).unwrap_or(head);
        if title.trim().is_empty() { return Err(ApiError::validation(422, "Validation Failed", "PullRequest", "missing_field", Some("title"))); }
        let head_tip = repo.branch_tip(head).cloned().ok_or_else(|| ApiError::validation(422, "Validation Failed", "PullRequest", "invalid", Some("head")))?;
        let base_tip = repo.branch_tip(base).cloned().ok_or_else(|| ApiError::validation(422, "Validation Failed", "PullRequest", "invalid", Some("base")))?;
        if repo.pulls.values().any(|other| other.state == "open" && other.head == head && other.base == base) {
            let mut error = ApiError::validation(422, "Validation Failed", "PullRequest", "custom", None);
            if let Some(detail) = error.errors.first_mut() { detail.message = Some(format!("A pull request already exists for {}:{head}.", self.user.login)); }
            return Err(error);
        }
        if self.is_ancestor(repo, &head_tip, &base_tip) {
            let mut error = ApiError::validation(422, "Validation Failed", "PullRequest", "custom", None);
            if let Some(detail) = error.errors.first_mut() { detail.message = Some(format!("No commits between {base} and {head}")); }
            return Err(error);
        }
        let now = now_iso();
        let pull = Pull {
            id: self.next_pull_id, number: repo.next_number, title: title.to_string(), body: body.to_string(),
            state: "open".to_string(), head: head.to_string(), base: base.to_string(), head_sha: head_tip,
            base_sha: base_tip, merged: false, merged_at: None, merge_commit_sha: None, comments: Vec::new(),
            created_at: now.clone(), updated_at: now, closed_at: None,
        };
        self.next_pull_id = self.next_pull_id.saturating_add(1);
        repo.next_number = repo.next_number.saturating_add(1);
        repo.pulls.insert(pull.number, pull.clone());
        self.sync_repo(repo);
        Ok(pull)
    }

    pub fn get_pull(&self, repo: &Repo, number: u64) -> Option<Pull> { repo.pulls.get(&number).cloned() }

    pub fn list_pulls(&self, repo: &Repo, state: Option<&str>) -> Vec<Pull> {
        let state = state.unwrap_or("open");
        repo.pulls.values().filter(|pull| state != "open" && state != "closed" || pull.state == state).cloned().collect()
    }

    pub fn update_pull(&mut self, repo: &mut Repo, number: u64, patch: PullUpdate) -> Result<Pull, ApiError> {
        let mut pull = repo.pulls.get(&number).cloned().ok_or_else(|| ApiError::new(404, "Not Found"))?;
        if let Some(title) = patch.title.filter(|title| !title.trim().is_empty()) { pull.title = title; }
        if let Some(body) = patch.body { pull.body = body; }
        if let Some(base) = patch.base.filter(|base| *base != pull.base) {
            if repo.branch_tip(&base).is_none() { return Err(ApiError::validation(422, "Validation Failed", "PullRequest", "invalid", Some("base"))); }
            pull.base = base;
        }
        if let Some(state) = patch.state.filter(|state| state == "open" || state == "closed") {
            if !pull.merged {
                if state == "closed" && pull.state == "open" {
                    let tips = self.pull_tips(repo, &pull);
                    pull.head_sha = tips.0;
                    pull.base_sha = tips.1;
                    pull.closed_at = Some(now_iso());
                }
                if state == "open" { pull.closed_at = None; }
                pull.state = state;
            }
        }
        pull.updated_at = now_iso();
        repo.pulls.insert(number, pull.clone());
        self.sync_repo(repo);
        Ok(pull)
    }

    pub fn pull_tips(&self, repo: &Repo, pull: &Pull) -> (String, String) {
        if pull.state == "closed" { return (pull.head_sha.clone(), pull.base_sha.clone()); }
        (repo.branch_tip(&pull.head).cloned().unwrap_or_else(|| pull.head_sha.clone()),
         repo.branch_tip(&pull.base).cloned().unwrap_or_else(|| pull.base_sha.clone()))
    }

    pub fn mergeable(&mut self, repo: &Repo, pull: &Pull) -> bool {
        if pull.state != "open" { return false; }
        let (head, base) = self.pull_tips(repo, pull);
        let key = (repo.id, base.clone(), head.clone());
        if let Some(value) = self.mergeable_cache.get(&key) { return *value; }
        let value = self.merged_tree(repo, &base, &head).is_some();
        self.mergeable_cache.insert(key, value);
        value
    }

    pub fn pull_files(&self, repo: &Repo, pull: &Pull) -> Result<Vec<FileDiff>, String> {
        let (head, base) = self.pull_tips(repo, pull);
        let merge_base = self.merge_base(repo, &base, &head);
        let before = self.flat_of_commit(repo, merge_base.as_deref())?;
        let after = self.flat_of_commit(repo, Some(&head))?;
        let paths: BTreeSet<String> = before.keys().chain(after.keys()).cloned().collect();
        let mut output = Vec::new();
        for path in paths {
            let old = before.get(&path);
            let new = after.get(&path);
            if old == new { continue; }
            let old_lines = old.and_then(|file| repo.get(&file.oid)).map(|object| lines_of(&object.data)).unwrap_or_default();
            let new_lines = new.and_then(|file| repo.get(&file.oid)).map(|object| lines_of(&object.data)).unwrap_or_default();
            let (additions, deletions) = diff_counts(&old_lines, &new_lines);
            let sha = new.or(old).map(|file| file.oid.clone()).unwrap_or_default();
            output.push(FileDiff {
                path,
                additions,
                deletions,
                status: if old.is_none() { "added" } else if new.is_none() { "removed" } else { "modified" }.to_string(),
                sha,
                patch: format!("@@ -{},{} +{},{} @@", if old_lines.is_empty() { 0 } else { 1 }, old_lines.len(), if new_lines.is_empty() { 0 } else { 1 }, new_lines.len()),
            });
        }
        Ok(output)
    }

    pub fn pull_commits(&self, repo: &Repo, pull: &Pull) -> Vec<String> {
        let (head, base) = self.pull_tips(repo, pull);
        self.log(repo, &head, Some(&base), 0, None).into_iter().rev().collect()
    }

    pub fn merge_pull(&mut self, repo: &mut Repo, number: u64, options: MergeOptions) -> Result<String, ApiError> {
        let mut pull = repo.pulls.get(&number).cloned().ok_or_else(|| ApiError::new(404, "Not Found"))?;
        if pull.merged || pull.state != "open" { return Err(ApiError::new(405, "Pull Request is not mergeable")); }
        let method = options.method.as_deref().filter(|value| !value.is_empty()).unwrap_or("merge").to_ascii_lowercase();
        if !["merge", "squash", "rebase"].contains(&method.as_str()) {
            return Err(ApiError::validation(422, "Validation Failed", "PullRequest", "invalid", Some("merge_method")));
        }
        let (head, base) = self.pull_tips(repo, &pull);
        if options.sha.as_deref().is_some_and(|sha| !sha.is_empty() && sha != head) {
            return Err(ApiError::new(409, "Head branch was modified. Review and try the merge again."));
        }
        let merged = self.merged_tree(repo, &base, &head).ok_or_else(|| ApiError::new(405, "Pull Request is not mergeable"))?;
        let default_title = if method == "merge" { format!("Merge pull request #{} from {}/{}", pull.number, self.user.login, pull.head) }
            else { format!("{} (#{})", pull.title, pull.number) };
        let title = options.title.filter(|title| !title.is_empty()).unwrap_or(default_title);
        let extra = options.message.filter(|message| !message.is_empty()).unwrap_or_else(|| if method == "merge" { pull.title.clone() } else { String::new() });
        let message = if extra.is_empty() { title } else { format!("{title}\n\n{extra}") };
        let sha = if method == "rebase" {
            let mut tip = base.clone();
            for oid in self.log(repo, &head, Some(&base), 0, None).into_iter().rev() {
                let Some(commit) = self.commit_of(repo, &oid) else { continue; };
                let parent = commit.parents.first().cloned();
                let base_tree = self.flat_of_commit(repo, parent.as_deref()).map_err(|error| ApiError::new(500, error))?;
                let ours = self.flat_of_commit(repo, Some(&tip)).map_err(|error| ApiError::new(500, error))?;
                let theirs = self.flat_of_commit(repo, Some(&oid)).map_err(|error| ApiError::new(500, error))?;
                let replayed = merge3(&base_tree, &ours, &theirs).ok_or_else(|| ApiError::new(405, "Pull Request is not mergeable"))?;
                let tree = self.build_tree(repo, &replayed)?;
                tip = self.make_commit(repo, &tree, &[tip], &commit.message, Some(parse_signature_line(&commit.author)));
            }
            tip
        } else {
            let tree = self.build_tree(repo, &merged)?;
            let parents = if method == "merge" { vec![base.clone(), head.clone()] } else { vec![base.clone()] };
            self.commit_tree(repo, &parents, &tree, &message)
        };
        self.write_ref(repo, &format!("refs/heads/{}", pull.base), &sha);
        let now = now_iso();
        pull.state = "closed".to_string();
        pull.merged = true;
        pull.merged_at = Some(now.clone());
        pull.closed_at = Some(now.clone());
        pull.updated_at = now;
        pull.merge_commit_sha = Some(sha.clone());
        pull.head_sha = head;
        pull.base_sha = base;
        repo.pulls.insert(number, pull);
        self.sync_repo(repo);
        Ok(sha)
    }

    fn merged_tree(&self, repo: &Repo, base_tip: &str, head: &str) -> Option<BTreeMap<String, FileEntry>> {
        let merge_base = self.merge_base(repo, base_tip, head);
        if merge_base.as_deref() == Some(base_tip) { return self.flat_of_commit(repo, Some(head)).ok(); }
        let base = self.flat_of_commit(repo, merge_base.as_deref()).ok()?;
        let ours = self.flat_of_commit(repo, Some(base_tip)).ok()?;
        let theirs = self.flat_of_commit(repo, Some(head)).ok()?;
        merge3(&base, &ours, &theirs)
    }

    pub fn adopt_default_branch(&mut self, repo: &mut Repo, created_branches: &[String]) {
        if repo.branch_tip(&repo.default_branch).is_some() { return; }
        if let Some(first) = created_branches.first() { repo.default_branch = first.clone(); self.sync_repo(repo); }
    }

    pub fn repo_json(&self, repo: &Repo) -> String {
        let open = repo.issues.values().filter(|issue| issue.state == "open").count();
        format!("{{\"id\":{},\"name\":{},\"full_name\":{},\"private\":{},\"description\":{},\"created_at\":{},\"updated_at\":{},\"pushed_at\":{},\"has_issues\":{},\"default_branch\":{},\"open_issues_count\":{},\"open_issues\":{},\"visibility\":{}}}",
            repo.id, json_quote(&repo.name), json_quote(&format!("{}/{}", self.user.login, repo.name)), repo.is_private,
            option_json(repo.description.as_deref()), json_quote(&repo.created_at), json_quote(&repo.updated_at),
            option_json(repo.pushed_at.as_deref().or(Some(&repo.created_at))), repo.has_issues, json_quote(&repo.default_branch),
            open, open, json_quote(if repo.is_private { "private" } else { "public" }))
    }

    pub fn issue_json(&self, repo: &Repo, issue: &Issue) -> String {
        let labels = issue.labels.iter().map(|name| {
            repo.labels.get(&name.to_lowercase()).map(|label| format!("{{\"id\":{},\"name\":{},\"color\":{},\"description\":{}}}", label.id, json_quote(&label.name), json_quote(&label.color), option_json(label.description.as_deref())))
                .unwrap_or_else(|| format!("{{\"name\":{}}}", json_quote(name)))
        }).collect::<Vec<_>>().join(",");
        format!("{{\"id\":{},\"number\":{},\"title\":{},\"body\":{},\"state\":{},\"state_reason\":{},\"labels\":[{}],\"comments\":{},\"created_at\":{},\"updated_at\":{},\"closed_at\":{}}}",
            issue.id, issue.number, json_quote(&issue.title), json_quote(&issue.body), json_quote(&issue.state),
            option_json(issue.state_reason.as_deref()), labels, issue.comments.len(), json_quote(&issue.created_at), json_quote(&issue.updated_at), option_json(issue.closed_at.as_deref()))
    }

    pub fn pull_json(&mut self, repo: &Repo, pull: &Pull) -> String {
        let (head, base) = self.pull_tips(repo, pull);
        let mergeable = self.mergeable(repo, pull);
        let mergeable_json = if pull.state == "open" { mergeable.to_string() } else { "null".to_string() };
        let mergeable_state = if pull.state != "open" { "unknown" } else if mergeable { "clean" } else { "dirty" };
        let files = self.pull_files(repo, pull).unwrap_or_default();
        let additions: usize = files.iter().map(|file| file.additions).sum();
        let deletions: usize = files.iter().map(|file| file.deletions).sum();
        format!("{{\"id\":{},\"number\":{},\"title\":{},\"body\":{},\"state\":{},\"head\":{{\"ref\":{},\"sha\":{}}},\"base\":{{\"ref\":{},\"sha\":{}}},\"merged\":{},\"merged_at\":{},\"merge_commit_sha\":{},\"mergeable\":{},\"mergeable_state\":{},\"comments\":{},\"commits\":{},\"additions\":{},\"deletions\":{},\"changed_files\":{},\"created_at\":{},\"updated_at\":{},\"closed_at\":{}}}",
            pull.id, pull.number, json_quote(&pull.title), json_quote(&pull.body), json_quote(&pull.state),
            json_quote(&pull.head), json_quote(&head), json_quote(&pull.base), json_quote(&base), pull.merged,
            option_json(pull.merged_at.as_deref()), option_json(pull.merge_commit_sha.as_deref()), mergeable_json,
            json_quote(mergeable_state), pull.comments.len(), self.pull_commits(repo, pull).len(),
            additions, deletions, files.len(), json_quote(&pull.created_at), json_quote(&pull.updated_at), option_json(pull.closed_at.as_deref()))
    }
}

pub fn now_iso() -> String { iso_from_unix(next_unix_seconds()) }

fn next_unix_seconds() -> u64 { BASE_UNIX_SECONDS.saturating_add(LOGICAL_CLOCK.fetch_add(1, Ordering::Relaxed)) }

fn iso_from_unix(seconds: u64) -> String {
    let days = (seconds / 86_400) as i64;
    let day_seconds = seconds % 86_400;
    let (year, month, day) = civil_from_days(days);
    format!("{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z", day_seconds / 3600, (day_seconds % 3600) / 60, day_seconds % 60)
}

fn civil_from_days(days_since_epoch: i64) -> (i64, i64, i64) {
    let z = days_since_epoch + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let day_of_era = z - era * 146_097;
    let year_of_era = (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let mut year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_prime = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_prime + 2) / 5 + 1;
    let month = month_prime + if month_prime < 10 { 3 } else { -9 };
    year += i64::from(month <= 2);
    (year, month, day)
}

fn valid_repo_name(name: &str) -> bool {
    !name.is_empty() && name.len() <= 100 && name.bytes().all(|byte| byte.is_ascii_alphanumeric() || b"._-".contains(&byte))
}

fn valid_new_repo_name(name: &str) -> bool {
    valid_repo_name(name) && name != "." && name != ".." && !name.to_ascii_lowercase().ends_with(".git")
}

fn valid_hex_color(color: &str) -> bool {
    let value = color.strip_prefix('#').unwrap_or(color);
    value.len() == 6 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn normalize_path(path: &str) -> Result<String, ApiError> {
    let segments: Vec<&str> = path.split('/').filter(|segment| !segment.is_empty()).collect();
    if segments.is_empty() || segments.iter().any(|segment| *segment == "." || *segment == ".." || *segment == ".git") {
        return Err(ApiError::new(422, "Invalid request.\n\npath is invalid."));
    }
    Ok(segments.join("/"))
}

fn committer_seconds(signature: &str) -> i64 {
    let mut parts = signature.rsplit(' ');
    let _timezone = parts.next();
    parts.next().and_then(|value| value.parse::<i64>().ok()).unwrap_or(0)
}

fn parse_signature_line(text: &str) -> Signature {
    let fallback = || Signature { name: text.to_string(), email: String::new(), time: 0, tz: "+0000".to_string() };
    let Some((before_tz, timezone)) = text.rsplit_once(' ') else { return fallback(); };
        if timezone.len() != 5 || !matches!(timezone.as_bytes().first(), Some(b'+') | Some(b'-'))
            || !timezone.as_bytes()[1..].iter().all(u8::is_ascii_digit) { return fallback(); }
    let Some((identity, time_text)) = before_tz.rsplit_once(' ') else { return fallback(); };
    let Ok(time) = time_text.parse::<i64>() else { return fallback(); };
    let Some((name, email_end)) = identity.rsplit_once(" <") else { return fallback(); };
    let Some(email) = email_end.strip_suffix('>') else { return fallback(); };
    Signature { name: name.to_string(), email: email.to_string(), time, tz: timezone.to_string() }
}

fn merge3(base: &BTreeMap<String, FileEntry>, ours: &BTreeMap<String, FileEntry>, theirs: &BTreeMap<String, FileEntry>) -> Option<BTreeMap<String, FileEntry>> {
    let paths: BTreeSet<String> = base.keys().chain(ours.keys()).chain(theirs.keys()).cloned().collect();
    let mut output = BTreeMap::new();
    for path in paths {
        let b = base.get(&path);
        let o = ours.get(&path);
        let t = theirs.get(&path);
        let same = |left: Option<&FileEntry>, right: Option<&FileEntry>| left == right;
        let picked = if same(o, t) { o } else if same(o, b) { t } else if same(t, b) { o } else { return None; };
        if let Some(file) = picked { output.insert(path, file.clone()); }
    }
    Some(output)
}

fn lines_of(data: &[u8]) -> Vec<String> {
    if data.is_empty() { return Vec::new(); }
    let text = String::from_utf8_lossy(data);
    let mut lines: Vec<String> = text.split('\n').map(str::to_string).collect();
    if lines.last().is_some_and(String::is_empty) { lines.pop(); }
    lines
}

fn diff_counts(before: &[String], after: &[String]) -> (usize, usize) {
    if before.len().saturating_mul(after.len()) <= 4_000_000 {
        let mut previous = vec![0usize; after.len() + 1];
        let mut current = vec![0usize; after.len() + 1];
        for left in before {
            for (index, right) in after.iter().enumerate() {
                current[index + 1] = if left == right { previous[index] + 1 } else { previous[index + 1].max(current[index]) };
            }
            std::mem::swap(&mut previous, &mut current);
            current.fill(0);
        }
        let common = previous[after.len()];
        (after.len() - common, before.len() - common)
    } else {
        let mut counts = HashMap::<&str, usize>::new();
        for line in before { *counts.entry(line).or_default() += 1; }
        let mut common = 0;
        for line in after {
            if let Some(count) = counts.get_mut(line.as_str()) { if *count > 0 { common += 1; *count -= 1; } }
        }
        (after.len() - common, before.len() - common)
    }
}

fn json_quote(value: &str) -> String {
    let mut output = String::with_capacity(value.len() + 2);
    output.push('"');
    for character in value.chars() {
        match character {
            '"' => output.push_str("\\\""),
            '\\' => output.push_str("\\\\"),
            '\n' => output.push_str("\\n"),
            '\r' => output.push_str("\\r"),
            '\t' => output.push_str("\\t"),
            character if character < ' ' => output.push_str(&format!("\\u{:04x}", character as u32)),
            character => output.push(character),
        }
    }
    output.push('"');
    output
}

fn option_json(value: Option<&str>) -> String { value.map(json_quote).unwrap_or_else(|| "null".to_string()) }

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repo_names_are_case_insensitive_and_deletion_releases_name() {
        let mut forge = Forge::new("Ada".into(), None);
        let repo = forge.create_repo("Example", None, false, true).expect("repo");
        assert!(forge.create_repo("example", None, false, true).is_err());
        let mut repo = repo;
        forge.delete_repo(&mut repo);
        assert!(forge.repo_by_name("ada", "EXAMPLE").is_none());
        assert_eq!(forge.create_repo("example", None, false, true).expect("recreated").id, 1002);
    }

    #[test]
    fn contents_commits_use_git_object_encoding_and_update_ref() {
        let mut forge = Forge::new("ada".into(), None);
        let mut repo = forge.create_repo("r", None, false, true).expect("repo");
        let (commit, blob) = forge.contents_commit(&mut repo, "a.txt", b"hello\n", "first", None, None).expect("write");
        assert_eq!(repo.branch_tip("main"), Some(&commit));
        assert_eq!(repo.get(&blob).map(|object| object.object_type), Some(ObjectType::Blob));
        assert_eq!(forge.commit_of(&repo, &commit).map(|value| value.message), Some("first\n".to_string()));
    }

    #[test]
    fn label_validation_and_shared_issue_pull_numbers() {
        let mut forge = Forge::new("ada".into(), None);
        let mut repo = forge.create_repo("r", None, false, true).expect("repo");
        assert!(forge.create_issue(&mut repo, "issue", "", &["bug".to_string()]).is_err());
        forge.create_label(&mut repo, "bug", None, None).expect("label");
        let issue = forge.create_issue(&mut repo, "issue", "", &["BUG".to_string()]).expect("issue");
        assert_eq!(issue.number, 1);
        assert_eq!(repo.next_number, 2);
    }
}
