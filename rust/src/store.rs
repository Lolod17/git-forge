use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicU64, Ordering};

static LOGICAL_CLOCK: AtomicU64 = AtomicU64::new(0);
const BASE_UNIX_SECONDS: u64 = 1_794_873_600; // 2026-11-17T00:00:00Z

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ObjectType {
    Commit,
    Tree,
    Blob,
    Tag,
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
        }
    }
}

#[derive(Debug)]
pub struct Forge {
    pub login: String,
    pub token: Option<String>,
    pub repos: HashMap<String, Repo>,
    pub repos_by_id: BTreeMap<u64, Repo>,
    pub next_repo_id: u64,
    pub next_issue_id: u64,
    pub next_comment_id: u64,
    pub next_label_id: u64,
    pub next_pull_id: u64,
}

impl Forge {
    pub fn new(login: String, token: Option<String>) -> Self {
        Self {
            login,
            token,
            repos: HashMap::new(),
            repos_by_id: BTreeMap::new(),
            next_repo_id: 1001,
            next_issue_id: 1,
            next_comment_id: 1,
            next_label_id: 1,
            next_pull_id: 1,
        }
    }

    pub fn create_repo(&mut self, name: String) -> &Repo {
        let id = self.next_repo_id;
        self.next_repo_id += 1;
        let repo = Repo::new(id, name);
        self.repos.insert(repo.name.to_lowercase(), repo.clone());
        self.repos_by_id.insert(id, repo);
        self.repos_by_id.get(&id).expect("inserted repository")
    }
}

/// The logical timestamp advances one second for each timestamped state change, independent of wall time.
pub fn now_iso() -> String {
    let seconds = BASE_UNIX_SECONDS + LOGICAL_CLOCK.fetch_add(1, Ordering::Relaxed);
    let days = (seconds / 86_400) as i64;
    let day_seconds = seconds % 86_400;
    let (year, month, day) = civil_from_days(days);
    let hour = day_seconds / 3_600;
    let minute = (day_seconds % 3_600) / 60;
    let second = day_seconds % 60;
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}Z")
}

fn civil_from_days(days_since_epoch: i64) -> (i64, i64, i64) {
    let z = days_since_epoch + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let day_of_era = z - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let mut year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_prime = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_prime + 2) / 5 + 1;
    let month = month_prime + if month_prime < 10 { 3 } else { -9 };
    year += i64::from(month <= 2);
    (year, month, day)
}
