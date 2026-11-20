//! GitHub-shaped REST v3 handlers backed by the shared in-memory Forge store.
use crate::git;
use crate::http::{Request, Response};
use crate::store::{self, ApiError, Comment, FileDiff, Forge, Issue, IssueFilter, Label, Pull, Repo, RepoUpdate};
use serde_json::{json, Map, Value};
use std::collections::BTreeMap;

const DOCS: &str = "https://docs.github.com/rest";

pub fn handle(forge: &mut Forge, request: &Request, tail: &str, origin: &str) -> Response {
    let path = if tail.is_empty() { "/".to_string() } else { format!("/{tail}") };
    let body = if request.body.is_empty() {
        Value::Null
    } else {
        match serde_json::from_slice::<Value>(&request.body) {
            Ok(value) => value,
            Err(_) => return error(400, "Problems parsing JSON", None),
        }
    };
    match dispatch(forge, request, &path, origin, &body) {
        Ok(response) => response,
        Err(error_value) => {
            let message = error_value.message.clone();
            error(error_value.status, &message, Some(error_value))
        }
    }
}

fn dispatch(f: &mut Forge, req: &Request, path: &str, origin: &str, body: &Value) -> Result<Response, ApiError> {
    let method = req.method.as_str();
    let api = format!("{origin}/api/v3");
    let login = f.login.clone();

    if let Some(_) = route(method, "GET", "/meta", path) {
        return Ok(json_response(200, json!({"verifiable_password_authentication":false,"installed_version":"3.15.0","gitforge":{"version":"0.1.0"}})));
    }
    if route(method, "GET", "/rate_limit", path).is_some() {
        return Ok(json_response(200, json!({"resources":{"core":{"limit":5000,"remaining":5000,"reset":0,"used":0}},"rate":{"limit":5000,"remaining":5000,"reset":0,"used":0}})));
    }
    if route(method, "GET", "/user", path).is_some() {
        let mut user = user_json(f, origin);
        let object = user.as_object_mut().expect("user serializer returns object");
        object.insert("name".into(), json!(f.user.name));
        object.insert("email".into(), Value::Null);
        object.insert("public_repos".into(), json!(f.list_repos().len()));
        return Ok(json_response(200, user));
    }
    if let Some(m) = route(method, "GET", "/users/:login", path) {
        if !f.is_viewer(&m[0]) { return Err(not_found()); }
        let mut user = user_json(f, origin);
        user.as_object_mut().unwrap().insert("name".into(), json!(f.user.name));
        return Ok(json_response(200, user));
    }
    if let Some(m) = route(method, "GET", "/users/:login/repos", path) {
        if !f.is_viewer(&m[0]) { return Err(not_found()); }
        let items = f.list_repos().iter().map(|repo| repo_json(f, repo, origin)).collect();
        return Ok(paged(req, path, origin, items));
    }
    if route(method, "GET", "/user/repos", path).is_some() {
        let visibility = query(req, "visibility");
        let items = f.list_repos().into_iter().filter(|repo| match visibility.as_deref() {
            Some("private") => repo.is_private,
            Some("public") => !repo.is_private,
            _ => true,
        }).map(|repo| repo_json(f, &repo, origin)).collect();
        return Ok(paged(req, path, origin, items));
    }
    if route(method, "POST", "/user/repos", path).is_some() {
        let name = string(body, "name").unwrap_or_default();
        let description = optional_string(body, "description");
        let private = bool_value(body, "private").unwrap_or_else(|| string(body, "visibility") == Some("private".into()));
        let has_issues = bool_value(body, "has_issues").unwrap_or(true);
        let mut repo = f.create_repo(name, description, private, has_issues)?;
        if bool_value(body, "auto_init") == Some(true) {
            let content = format!("# {}\n{}", repo.name, repo.description.as_ref().map(|s| format!("\n{s}\n")).unwrap_or_default());
            f.contents_commit(&mut repo, "README.md", content.as_bytes(), "Initial commit", None, None)?;
        }
        return Ok(json_response(201, repo_json(f, &repo, origin)));
    }

    if let Some(m) = route(method, "GET", "/repos/:owner/:repo", path) {
        let repo = required_repo(f, &m[0], &m[1])?;
        return Ok(json_response(200, repo_json(f, &repo, origin)));
    }
    if let Some(m) = route(method, "PATCH", "/repos/:owner/:repo", path) {
        let mut repo = required_repo(f, &m[0], &m[1])?;
        let patch = RepoUpdate {
            name: string(body, "name"),
            description: body.get("description").map(|v| if v.is_null() { None } else { v.as_str().map(str::to_string) }),
            private: bool_value(body, "private"),
            visibility: string(body, "visibility"),
            has_issues: bool_value(body, "has_issues"),
            default_branch: string(body, "default_branch"),
        };
        repo = f.update_repo(&mut repo, patch)?;
        return Ok(json_response(200, repo_json(f, &repo, origin)));
    }
    if let Some(m) = route(method, "DELETE", "/repos/:owner/:repo", path) {
        let mut repo = required_repo(f, &m[0], &m[1])?;
        f.delete_repo(&mut repo);
        return Ok(empty_json(204));
    }

    // Branches and refs.
    if let Some(m) = route(method, "GET", "/repos/:owner/:repo/branches", path) {
        let repo = required_repo(f, &m[0], &m[1])?;
        let mut branches: Vec<_> = repo.refs.iter().filter(|(name, _)| name.starts_with("refs/heads/")).collect();
        branches.sort_by(|a,b| a.0.cmp(b.0));
        let items = branches.into_iter().map(|(reference, sha)| branch_json(f, &repo, &reference[11..], sha, origin)).collect();
        return Ok(paged(req, path, origin, items));
    }
    if let Some(m) = route(method, "GET", "/repos/:owner/:repo/branches/*branch", path) {
        let repo = required_repo(f, &m[0], &m[1])?;
        let branch = m[2].clone();
        let sha = repo.branch_tip(&branch).cloned().ok_or_else(|| ApiError::new(404, "Branch not found"))?;
        let mut result = branch_json(f, &repo, &branch, &sha, origin);
        result["commit"] = commit_json(f, &repo, &sha, origin)?;
        result["_links"] = json!({"self":format!("{api}/repos/{}/{}/branches/{}",login,repo.name,branch)});
        return Ok(json_response(200, result));
    }
    if let Some(m) = route(method, "GET", "/repos/:owner/:repo/git/refs", path) {
        let repo = required_repo(f, &m[0], &m[1])?;
        return Ok(ref_list(f, req, path, origin, &repo, "refs/"));
    }
    if let Some(m) = route(method, "GET", "/repos/:owner/:repo/git/matching-refs/*ref", path) {
        let repo = required_repo(f, &m[0], &m[1])?;
        let prefix = format!("refs/{}", m[2]);
        return Ok(ref_list(f, req, path, origin, &repo, &prefix));
    }
    for pattern in ["/repos/:owner/:repo/git/refs/*ref", "/repos/:owner/:repo/git/ref/*ref"] {
        if let Some(m) = route(method, "GET", pattern, path) {
            let repo = required_repo(f, &m[0], &m[1])?;
            let reference = format!("refs/{}", m[2]);
            let sha = repo.refs.get(&reference).ok_or_else(not_found)?;
            return Ok(json_response(200, ref_json(f, &repo, &reference, sha, origin)));
        }
    }
    if let Some(m) = route(method, "POST", "/repos/:owner/:repo/git/refs", path) {
        let mut repo = required_repo(f, &m[0], &m[1])?;
        let reference = string(body, "ref").unwrap_or_default();
        let sha = string(body, "sha").unwrap_or_default();
        if !reference.starts_with("refs/") || reference.split('/').count() < 3 { return Err(ApiError::new(422, "Reference name must start with 'refs/' and have at least two slashes.")); }
        if !git::is_oid(&sha) || repo.get(&sha).is_none() { return Err(ApiError::new(422, "Object does not exist")); }
        if repo.refs.contains_key(&reference) { return Err(ApiError::new(422, "Reference already exists")); }
        f.write_ref(&mut repo, &reference, &sha);
        return Ok(json_response(201, ref_json(f, &repo, &reference, &sha, origin)));
    }
    if let Some(m) = route(method, "PATCH", "/repos/:owner/:repo/git/refs/*ref", path) {
        let mut repo = required_repo(f, &m[0], &m[1])?;
        let reference = format!("refs/{}", m[2]);
        let current = repo.refs.get(&reference).cloned().ok_or_else(|| ApiError::new(422, "Reference does not exist"))?;
        let sha = string(body, "sha").unwrap_or_default();
        if !git::is_oid(&sha) || repo.get(&sha).is_none() { return Err(ApiError::new(422, "Object does not exist")); }
        if bool_value(body, "force") != Some(true) && !f.is_ancestor(&repo, &current, &sha) { return Err(ApiError::new(422, "Update is not a fast forward")); }
        f.write_ref(&mut repo, &reference, &sha);
        return Ok(json_response(200, ref_json(f, &repo, &reference, &sha, origin)));
    }
    if let Some(m) = route(method, "DELETE", "/repos/:owner/:repo/git/refs/*ref", path) {
        let mut repo = required_repo(f, &m[0], &m[1])?;
        let reference = format!("refs/{}", m[2]);
        if !repo.refs.contains_key(&reference) { return Err(ApiError::new(422, "Reference does not exist")); }
        f.delete_ref(&mut repo, &reference);
        return Ok(empty_json(204));
    }

    // Git object and commit APIs.
    if let Some(m) = route(method, "GET", "/repos/:owner/:repo/git/commits/:sha", path) {
        let repo = required_repo(f, &m[0], &m[1])?;
        let commit = commit_json(f, &repo, &m[2], origin)?;
        let inner = &commit["commit"];
        return Ok(json_response(200, json!({"sha":commit["sha"],"node_id":commit["node_id"],"url":inner["url"],"author":inner["author"],"committer":inner["committer"],"message":inner["message"],"tree":inner["tree"],"parents":commit["parents"]})));
    }
    if let Some(m) = route(method, "GET", "/repos/:owner/:repo/git/trees/*sha", path) {
        let repo = required_repo(f, &m[0], &m[1])?;
        let reference = &m[2];
        let resolved = f.resolve(&repo, reference);
        let tree_sha = resolved.as_deref().and_then(|sha| f.commit_of(&repo, sha).map(|c| c.tree)).unwrap_or_else(|| reference.clone());
        if repo.get(&tree_sha).is_none_or(|object| object.object_type != store::ObjectType::Tree) { return Err(not_found()); }
        let recursive = query(req, "recursive").is_some_and(|value| value != "0" && value != "false");
        let mut entries = Vec::new();
        tree_walk(f, &repo, &tree_sha, "", recursive, origin, &mut entries)?;
        return Ok(json_response(200, json!({"sha":tree_sha,"url":format!("{api}/repos/{login}/{}/git/trees/{tree_sha}",repo.name),"tree":entries,"truncated":false})));
    }
    if let Some(m) = route(method, "GET", "/repos/:owner/:repo/git/blobs/:sha", path) {
        let repo = required_repo(f, &m[0], &m[1])?;
        let object = repo.get(&m[2]).filter(|o| o.object_type == store::ObjectType::Blob).ok_or_else(not_found)?;
        return Ok(json_response(200, json!({"sha":m[2],"node_id":node_id("B", &m[2]),"size":object.data.len(),"url":format!("{api}/repos/{login}/{}/git/blobs/{}",repo.name,m[2]),"content":base64_encode(&object.data),"encoding":"base64"})));
    }
    if let Some(m) = route(method, "GET", "/repos/:owner/:repo/commits", path) {
        let repo = required_repo(f, &m[0], &m[1])?;
        if repo.refs.is_empty() { return Err(ApiError::new(409, "Git Repository is empty.")); }
        let requested = query(req, "sha").unwrap_or_else(|| repo.default_branch.clone());
        let tip = f.resolve(&repo, &requested).ok_or_else(|| ApiError::new(404, format!("No commit found for SHA: {requested}")))?;
        let mut commits = f.log(&repo, &tip, None, 0, None);
        if let Some(filter_path) = query(req, "path").filter(|s| !s.is_empty()) {
            commits.retain(|oid| {
                let Some(commit) = f.commit_of(&repo, oid) else { return false; };
                let here = f.lookup_path(&repo, oid, &filter_path).ok().flatten().map(path_entry_oid);
                let before = commit.parents.first().and_then(|parent| f.lookup_path(&repo, parent, &filter_path).ok().flatten()).map(path_entry_oid);
                here != before
            });
        }
        let items = commits.iter().map(|sha| commit_json(f, &repo, sha, origin)).collect::<Result<Vec<_>,_>>()?;
        return Ok(paged(req, path, origin, items));
    }
    if let Some(m) = route(method, "GET", "/repos/:owner/:repo/commits/*ref", path) {
        let repo = required_repo(f, &m[0], &m[1])?;
        let sha = f.resolve(&repo, &m[2]).ok_or_else(|| ApiError::new(422, format!("No commit found for SHA: {}", m[2])))?;
        let mut commit = commit_json(f, &repo, &sha, origin)?;
        let parsed = f.commit_of(&repo, &sha).ok_or_else(not_found)?;
        let before = parsed.parents.first().and_then(|parent| f.commit_of(&repo, parent)).and_then(|parent| f.flatten_tree(&repo, &parent.tree).ok()).unwrap_or_default();
        let after = f.flatten_tree(&repo, &parsed.tree).unwrap_or_default();
        let files = changed_files(&before, &after);
        commit["files"] = Value::Array(files);
        commit["stats"] = json!({"total":commit["files"].as_array().unwrap().len(),"additions":0,"deletions":0});
        return Ok(json_response(200, commit));
    }

    // Contents and README.
    if route(method, "GET", "/repos/:owner/:repo/contents", path).is_some() || route(method, "GET", "/repos/:owner/:repo/contents/*path", path).is_some() {
        let m = route(method, "GET", "/repos/:owner/:repo/contents/*path", path).or_else(|| route(method, "GET", "/repos/:owner/:repo/contents", path)).unwrap();
        let repo = required_repo(f, &m[0], &m[1])?;
        let file_path = m.get(2).cloned().unwrap_or_default().trim_matches('/').to_string();
        return contents_read(f, req, &repo, &file_path, origin);
    }
    if let Some(m) = route(method, "GET", "/repos/:owner/:repo/readme", path) {
        let repo = required_repo(f, &m[0], &m[1])?;
        let reference = query(req, "ref").unwrap_or_else(|| repo.default_branch.clone());
        let commit = f.resolve(&repo, &reference).ok_or_else(not_found)?;
        let root = f.lookup_path(&repo, &commit, "").ok().flatten();
        let entry = match root { Some(store::PathEntry::Directory { entries, .. }) => entries.into_iter().find(|entry| entry.mode != "40000" && (entry.name.to_ascii_lowercase() == "readme" || entry.name.to_ascii_lowercase().starts_with("readme."))), _ => None }.ok_or_else(not_found)?;
        return Ok(json_response(200, content_file(f, &repo, &entry.name, &entry.oid, true, origin)));
    }
    if let Some(m) = route(method, "PUT", "/repos/:owner/:repo/contents/*path", path) {
        let mut repo = required_repo(f, &m[0], &m[1])?;
        let file_path = m[2].clone();
        let message = string(body, "message").filter(|v| !v.is_empty()).ok_or_else(|| ApiError::new(422, "Invalid request.\n\n\"message\" wasn't supplied."))?;
        let encoded = string(body, "content").ok_or_else(|| ApiError::new(422, "Invalid request.\n\n\"content\" wasn't supplied."))?;
        let bytes = base64_decode(&encoded).ok_or_else(|| ApiError::new(422, "Invalid base64 content"))?;
        let branch = string(body, "branch").filter(|v| !v.is_empty());
        let expected = string(body, "sha");
        let tip = repo.branch_tip(branch.as_deref().unwrap_or(&repo.default_branch)).cloned();
        let existed = tip.as_deref().and_then(|tip| f.lookup_path(&repo, tip, &file_path).ok().flatten()).is_some_and(|entry| matches!(entry, store::PathEntry::File { .. }));
        let (commit, blob) = f.contents_commit(&mut repo, &file_path, &bytes, &message, branch.as_deref(), expected.as_deref())?;
        return Ok(json_response(if existed { 200 } else { 201 }, content_write(f, &repo, Some(&file_path), Some(&blob), &commit, origin)?));
    }
    if let Some(m) = route(method, "DELETE", "/repos/:owner/:repo/contents/*path", path) {
        let mut repo = required_repo(f, &m[0], &m[1])?;
        let message = string(body, "message").filter(|v| !v.is_empty()).ok_or_else(|| ApiError::new(422, "Invalid request.\n\n\"message\" wasn't supplied."))?;
        let branch = string(body, "branch").filter(|v| !v.is_empty());
        let commit = f.delete_contents(&mut repo, &m[2], &message, branch.as_deref(), string(body, "sha").as_deref())?;
        return Ok(json_response(200, content_write(f, &repo, None, None, &commit, origin)?));
    }

    // Labels.
    if let Some(m) = route(method, "GET", "/repos/:owner/:repo/labels", path) {
        let repo = required_repo(f, &m[0], &m[1])?;
        let items = f.list_labels(&repo).iter().map(|label| label_json(f, &repo, label, origin)).collect();
        return Ok(paged(req, path, origin, items));
    }
    if let Some(m) = route(method, "POST", "/repos/:owner/:repo/labels", path) {
        let mut repo = required_repo(f, &m[0], &m[1])?;
        let name = string(body, "name").unwrap_or_default();
        let color = string(body, "color");
        let label = f.create_label(&mut repo, &name, color.as_deref(), optional_string(body, "description"))?;
        return Ok(json_response(201, label_json(f, &repo, &label, origin)));
    }
    if let Some(m) = route(method, "GET", "/repos/:owner/:repo/labels/:name", path) {
        let repo = required_repo(f, &m[0], &m[1])?;
        let label = f.get_label(&repo, &m[2]).ok_or_else(not_found)?;
        return Ok(json_response(200, label_json(f, &repo, &label, origin)));
    }

    // Issues and comments.
    if let Some(m) = route(method, "GET", "/repos/:owner/:repo/issues", path) {
        let repo = required_repo(f, &m[0], &m[1])?;
        let filter = IssueFilter { state: query(req,"state").or(Some("open".into())), sort: query(req,"sort").or(Some("created".into())), direction: query(req,"direction").or(Some("desc".into())), labels: query(req,"labels").unwrap_or_default().split(',').filter(|s| !s.is_empty()).map(str::to_string).collect() };
        let items = f.list_issues(&repo, &filter).iter().map(|issue| issue_json(f, &repo, issue, origin)).collect();
        return Ok(paged(req, path, origin, items));
    }
    if let Some(m) = route(method, "POST", "/repos/:owner/:repo/issues", path) {
        let mut repo = required_repo(f, &m[0], &m[1])?;
        if !repo.has_issues { return Err(ApiError::new(410, "Issues are disabled for this repo")); }
        let labels = string_array(body.get("labels"));
        let issue = f.create_issue(&mut repo, string(body,"title").unwrap_or_default().as_str(), string(body,"body").unwrap_or_default().as_str(), &labels)?;
        return Ok(json_response(201, issue_json(f, &repo, &issue, origin)));
    }
    if let Some(m) = route(method, "GET", "/repos/:owner/:repo/issues/:number", path) {
        let repo = required_repo(f, &m[0], &m[1])?;
        let issue = issue_by_number(&repo, &m[2])?;
        return Ok(json_response(200, issue_json(f, &repo, &issue, origin)));
    }
    if let Some(m) = route(method, "PATCH", "/repos/:owner/:repo/issues/:number", path) {
        let mut repo = required_repo(f, &m[0], &m[1])?;
        let number = parse_number(&m[2])?;
        if repo.issues.get(&number).is_none() { return Err(not_found()); }
        let labels = body.get("labels").map(|value| string_array(Some(value)));
        let updated = f.update_issue(&mut repo, number, store::IssueUpdate { title: string(body,"title"), body: optional_string(body,"body"), labels, state: string(body,"state"), state_reason: string(body,"state_reason") })?;
        return Ok(json_response(200, issue_json(f, &repo, &updated, origin)));
    }
    if let Some(m) = route(method, "GET", "/repos/:owner/:repo/issues/:number/comments", path) {
        let repo = required_repo(f, &m[0], &m[1])?;
        let number = parse_number(&m[2])?;
        if !repo.issues.contains_key(&number) && !repo.pulls.contains_key(&number) { return Err(not_found()); }
        let items = f.list_comments(&repo, number).iter().map(|comment| comment_json(f, &repo, number, comment, origin)).collect();
        return Ok(paged(req, path, origin, items));
    }
    if let Some(m) = route(method, "POST", "/repos/:owner/:repo/issues/:number/comments", path) {
        let mut repo = required_repo(f, &m[0], &m[1])?;
        let number = parse_number(&m[2])?;
        if !repo.issues.contains_key(&number) && !repo.pulls.contains_key(&number) { return Err(not_found()); }
        let comment = f.add_comment(&mut repo, number, string(body,"body").unwrap_or_default().as_str())?;
        return Ok(json_response(201, comment_json(f, &repo, number, &comment, origin)));
    }
    if let Some(m) = route(method, "GET", "/repos/:owner/:repo/issues/:number/labels", path) {
        let repo = required_repo(f, &m[0], &m[1])?;
        return Ok(json_response(200, json!(issue_by_number(&repo, &m[2])?.labels)));
    }
    if let Some(m) = route(method, "POST", "/repos/:owner/:repo/issues/:number/labels", path) {
        let mut repo = required_repo(f, &m[0], &m[1])?;
        let number = parse_number(&m[2])?;
        let issue = repo.issues.get(&number).cloned().ok_or_else(not_found)?;
        let wanted = if body.is_array() { string_array(Some(body)) } else { string_array(body.get("labels")) };
        let names = f.label_names(&repo, &wanted)?;
        let mut merged = issue.labels.clone();
        for name in names { if !merged.iter().any(|old| old.eq_ignore_ascii_case(&name)) { merged.push(name); } }
        let updated = f.update_issue(&mut repo, number, store::IssueUpdate { labels: Some(merged), ..Default::default() })?;
        return Ok(json_response(200, json!(updated.labels)));
    }

    // Pull requests.
    if let Some(m) = route(method, "GET", "/repos/:owner/:repo/pulls", path) {
        let repo = required_repo(f, &m[0], &m[1])?;
        let state = query(req,"state").unwrap_or_else(|| "open".into());
        let mut pulls = f.list_pulls(&repo, Some(&state));
        if let Some(head) = query(req,"head").filter(|s| !s.is_empty()) { let branch = head.split_once(':').map(|(_,b)| b).unwrap_or(&head); pulls.retain(|pull| pull.head == branch); }
        if let Some(base) = query(req,"base").filter(|s| !s.is_empty()) { pulls.retain(|pull| pull.base == base); }
        if query(req,"direction").as_deref() == Some("asc") { pulls.sort_by_key(|pull| pull.number); } else { pulls.sort_by_key(|pull| std::cmp::Reverse(pull.number)); }
        let items = pulls.iter().map(|pull| pull_json(f, &repo, pull, origin, false)).collect::<Result<Vec<_>,_>>()?;
        return Ok(paged(req, path, origin, items));
    }
    if let Some(m) = route(method, "POST", "/repos/:owner/:repo/pulls", path) {
        let mut repo = required_repo(f, &m[0], &m[1])?;
        let pull = f.create_pull(&mut repo, string(body,"title").unwrap_or_default().as_str(), string(body,"body").unwrap_or_default().as_str(), string(body,"head").unwrap_or_default().as_str(), string(body,"base").unwrap_or_default().as_str())?;
        return Ok(json_response(201, pull_json(f, &repo, &pull, origin, true)?));
    }
    if let Some(m) = route(method, "GET", "/repos/:owner/:repo/pulls/:number", path) {
        let repo = required_repo(f, &m[0], &m[1])?;
        let pull = pull_by_number(&repo, &m[2])?;
        return Ok(json_response(200, pull_json(f, &repo, &pull, origin, true)?));
    }
    if let Some(m) = route(method, "PATCH", "/repos/:owner/:repo/pulls/:number", path) {
        let mut repo = required_repo(f, &m[0], &m[1])?;
        let number = parse_number(&m[2])?;
        let updated = f.update_pull(&mut repo, number, store::PullUpdate { title: string(body,"title"), body: optional_string(body,"body"), base: string(body,"base"), state: string(body,"state") })?;
        return Ok(json_response(200, pull_json(f, &repo, &updated, origin, true)?));
    }
    if let Some(m) = route(method, "GET", "/repos/:owner/:repo/pulls/:number/files", path) {
        let repo = required_repo(f, &m[0], &m[1])?;
        let pull = pull_by_number(&repo, &m[2])?;
        let files = f.pull_files(&repo, &pull).map_err(|e| ApiError::new(500,e))?;
        let items = files.iter().map(|file| pull_file_json(file, &repo, origin, &login, &api)).collect();
        return Ok(paged(req, path, origin, items));
    }
    if let Some(m) = route(method, "GET", "/repos/:owner/:repo/pulls/:number/commits", path) {
        let repo = required_repo(f, &m[0], &m[1])?;
        let pull = pull_by_number(&repo, &m[2])?;
        let commits = f.pull_commits(&repo, &pull);
        let items = commits.iter().map(|sha| commit_json(f, &repo, sha, origin)).collect::<Result<Vec<_>,_>>()?;
        return Ok(paged(req, path, origin, items));
    }
    if let Some(m) = route(method, "GET", "/repos/:owner/:repo/pulls/:number/merge", path) {
        let repo = required_repo(f, &m[0], &m[1])?;
        if !pull_by_number(&repo, &m[2])?.merged { return Err(not_found()); }
        return Ok(empty_json(204));
    }
    if let Some(m) = route(method, "PUT", "/repos/:owner/:repo/pulls/:number/merge", path) {
        let mut repo = required_repo(f, &m[0], &m[1])?;
        let number = parse_number(&m[2])?;
        let result = f.merge_pull(&mut repo, number, store::MergeOptions { method: string(body,"merge_method"), title: string(body,"commit_title"), message: string(body,"commit_message"), sha: string(body,"sha") })?;
        return Ok(json_response(200, json!({"sha":result,"merged":true,"message":"Pull Request successfully merged"})));
    }

    Err(not_found())
}

fn route(method: &str, wanted_method: &str, pattern: &str, path: &str) -> Option<Vec<String>> {
    if method != wanted_method && !(method == "HEAD" && wanted_method == "GET") { return None; }
    let expected: Vec<&str> = pattern.trim_matches('/').split('/').filter(|s| !s.is_empty()).collect();
    let actual: Vec<&str> = path.trim_matches('/').split('/').filter(|s| !s.is_empty()).collect();
    let wildcard = expected.last().is_some_and(|part| part.starts_with('*'));
    if (!wildcard && expected.len() != actual.len()) || actual.len() < expected.len().saturating_sub(usize::from(wildcard)) { return None; }
    let fixed = if wildcard { expected.len() - 1 } else { expected.len() };
    let mut captures = Vec::new();
    for (left, right) in expected[..fixed].iter().zip(actual.iter()) {
        if let Some(_name) = left.strip_prefix(':') { if right.is_empty() { return None; } captures.push((*right).to_string()); }
        else if left.starts_with('*') || left != right { return None; }
    }
    if wildcard {
        if actual.len() == fixed { return None; }
        captures.push(actual[fixed..].join("/"));
    }
    Some(captures)
}

fn query(req: &Request, key: &str) -> Option<String> { req.query.iter().rev().find(|(name,_)| name == key).map(|(_,value)| value.clone()) }
fn string(value: &Value, key: &str) -> Option<String> { value.get(key).and_then(Value::as_str).map(str::to_string) }
fn optional_string(value: &Value, key: &str) -> Option<String> { value.get(key).and_then(|v| if v.is_null() { None } else { v.as_str() }).map(str::to_string) }
fn bool_value(value: &Value, key: &str) -> Option<bool> { value.get(key).and_then(Value::as_bool) }
fn string_array(value: Option<&Value>) -> Vec<String> { value.and_then(Value::as_array).map(|array| array.iter().filter_map(Value::as_str).map(str::to_string).collect()).unwrap_or_default() }
fn parse_number(value: &str) -> Result<u64,ApiError> { value.parse().ok().filter(|n| *n > 0).ok_or_else(not_found) }
fn not_found() -> ApiError { ApiError::new(404,"Not Found") }

fn required_repo(f: &Forge, owner: &str, name: &str) -> Result<Repo,ApiError> { f.repo_by_name(owner,name).ok_or_else(not_found) }
fn issue_by_number(repo: &Repo, raw: &str) -> Result<Issue,ApiError> { repo.issues.get(&parse_number(raw)?).cloned().ok_or_else(not_found) }
fn pull_by_number(repo: &Repo, raw: &str) -> Result<Pull,ApiError> { repo.pulls.get(&parse_number(raw)?).cloned().ok_or_else(not_found) }

fn json_response(status: u16, value: Value) -> Response { Response::send_json_bytes(status, serde_json::to_vec(&value).unwrap_or_else(|_| b"{}".to_vec())) }
fn empty_json(status: u16) -> Response { Response::send_json_bytes(status, Vec::new()) }
fn error(status: u16, message: &str, api_error: Option<ApiError>) -> Response {
    let mut result = json!({"message":message,"documentation_url":DOCS});
    if let Some(api_error) = api_error {
        if !api_error.errors.is_empty() {
            let errors: Vec<Value> = api_error.errors.into_iter().map(|item| {
                let mut value = Map::new(); value.insert("resource".into(),json!(item.resource)); value.insert("code".into(),json!(item.code));
                if let Some(field)=item.field { value.insert("field".into(),json!(field)); }
                if let Some(message)=item.message { value.insert("message".into(),json!(message)); }
                if let Some(value_string)=item.value { value.insert("value".into(),json!(value_string)); }
                Value::Object(value)
            }).collect();
            result["errors"] = json!(errors);
        }
    }
    json_response(status,result)
}

fn paged(req: &Request, path: &str, origin: &str, items: Vec<Value>) -> Response {
    let per_page = query(req,"per_page").and_then(|s| s.parse::<usize>().ok()).filter(|n| *n>0).unwrap_or(30).min(100);
    let page = query(req,"page").and_then(|s| s.parse::<usize>().ok()).filter(|n| *n>0).unwrap_or(1);
    let start = page.saturating_sub(1).saturating_mul(per_page);
    let end = start.saturating_add(per_page).min(items.len());
    let values = if start < items.len() { items[start..end].to_vec() } else { Vec::new() };
    let last = ((items.len() + per_page - 1) / per_page).max(1);
    let mut links = Vec::new();
    let make_link = |number: usize| {
        let mut pairs: Vec<(String,String)> = req.query.iter().filter(|(key,_)| key != "page" && key != "per_page").cloned().collect();
        pairs.push(("per_page".into(),per_page.to_string())); pairs.push(("page".into(),number.to_string()));
        format!("<{origin}/api/v3{}?{}>", if path == "/" { String::new() } else { path.to_string() }, encode_query(&pairs))
    };
    if page < last { links.push(format!("{}; rel=\"next\"",make_link(page+1))); links.push(format!("{}; rel=\"last\"",make_link(last))); }
    if page > 1 { links.push(format!("{}; rel=\"first\"",make_link(1))); links.push(format!("{}; rel=\"prev\"",make_link((page-1).min(last)))); }
    let mut response = json_response(200,Value::Array(values));
    if !links.is_empty() { response.extra.push(("link".into(),links.join(", "))); }
    response
}

fn encode_query(pairs: &[(String,String)]) -> String { pairs.iter().map(|(k,v)|format!("{}={}",percent_encode(k),percent_encode(v))).collect::<Vec<_>>().join("&") }
fn percent_encode(value: &str) -> String { let mut out=String::new(); for byte in value.bytes() { if byte.is_ascii_alphanumeric() || b"-_.~".contains(&byte) { out.push(byte as char); } else { out.push_str(&format!("%{byte:02X}")); } } out }
fn encode_path(value: &str) -> String { value.split('/').map(percent_encode).collect::<Vec<_>>().join("/") }

fn user_json(f: &Forge, origin: &str) -> Value {
    let api=format!("{origin}/api/v3"); let login=&f.user.login;
    json!({"login":login,"id":f.user.id,"node_id":node_id("U",&f.user.id.to_string()),"avatar_url":format!("{origin}/avatars/{login}"),"gravatar_id":"","url":format!("{api}/users/{login}"),"html_url":format!("{origin}/{login}"),"repos_url":format!("{api}/users/{login}/repos"),"type":"User","site_admin":false})
}

fn repo_json(f: &Forge, repo: &Repo, origin: &str) -> Value {
    let api=format!("{origin}/api/v3"); let login=&f.user.login; let full=format!("{login}/{}",repo.name); let url=format!("{api}/repos/{full}");
    let open=repo.issues.values().filter(|issue|issue.state=="open").count();
    json!({"id":repo.id,"node_id":node_id("R",&repo.id.to_string()),"name":repo.name,"full_name":full,"private":repo.is_private,"owner":user_json(f,origin),"html_url":format!("{origin}/{full}"),"description":repo.description,"fork":false,"url":url,"clone_url":format!("{origin}/{full}.git"),"git_url":format!("{origin}/{full}.git"),"ssh_url":format!("{origin}/{full}.git"),"issues_url":format!("{url}/issues{{/number}}"),"pulls_url":format!("{url}/pulls{{/number}}"),"contents_url":format!("{url}/contents/{{+path}}"),"branches_url":format!("{url}/branches{{/branch}}"),"commits_url":format!("{url}/commits{{/sha}}"),"created_at":repo.created_at,"updated_at":repo.updated_at,"pushed_at":repo.pushed_at.as_deref().unwrap_or(&repo.created_at),"size":0,"stargazers_count":0,"watchers_count":0,"forks_count":0,"open_issues_count":open,"open_issues":open,"language":null,"has_issues":repo.has_issues,"has_projects":false,"has_wiki":false,"has_pages":false,"has_downloads":false,"archived":false,"disabled":false,"visibility":if repo.is_private {"private"} else {"public"},"default_branch":repo.default_branch,"permissions":{"admin":true,"maintain":true,"push":true,"triage":true,"pull":true},"allow_squash_merge":true,"allow_merge_commit":true,"allow_rebase_merge":true})
}

fn label_json(f: &Forge, repo: &Repo, label: &Label, origin: &str) -> Value {
    json!({"id":label.id,"node_id":node_id("LA",&label.id.to_string()),"url":format!("{origin}/api/v3/repos/{}/{}/labels/{}",f.login,repo.name,percent_encode(&label.name)),"name":label.name,"color":label.color,"description":label.description,"default":false})
}

fn issue_json(f: &Forge, repo: &Repo, issue: &Issue, origin: &str) -> Value {
    let api=format!("{origin}/api/v3"); let url=format!("{api}/repos/{}/{}/issues/{}",f.login,repo.name,issue.number);
    let labels: Vec<Value>=issue.labels.iter().filter_map(|name|f.get_label(repo,name)).map(|label|label_json(f,repo,&label,origin)).collect();
    json!({"id":issue.id,"node_id":node_id("I",&issue.id.to_string()),"url":url,"repository_url":format!("{api}/repos/{}/{}",f.login,repo.name),"comments_url":format!("{url}/comments"),"html_url":format!("{origin}/{}/{}/issues/{}",f.login,repo.name,issue.number),"number":issue.number,"state":issue.state,"state_reason":issue.state_reason,"title":issue.title,"body":issue.body,"user":user_json(f,origin),"labels":labels,"assignee":null,"assignees":[],"milestone":null,"locked":false,"comments":issue.comments.len(),"created_at":issue.created_at,"updated_at":issue.updated_at,"closed_at":issue.closed_at,"author_association":"OWNER"})
}

fn comment_json(f:&Forge, repo:&Repo, number:u64, comment:&Comment, origin:&str)->Value {
    json!({"id":comment.id,"node_id":node_id("IC",&comment.id.to_string()),"url":format!("{origin}/api/v3/repos/{}/{}/issues/comments/{}",f.login,repo.name,comment.id),"html_url":format!("{origin}/{}/{}/issues/{}#issuecomment-{}",f.login,repo.name,number,comment.id),"issue_url":format!("{origin}/api/v3/repos/{}/{}/issues/{number}",f.login,repo.name),"body":comment.body,"user":user_json(f,origin),"created_at":comment.created_at,"updated_at":comment.updated_at,"author_association":"OWNER"})
}

fn branch_json(f:&Forge,repo:&Repo,name:&str,sha:&str,origin:&str)->Value { json!({"name":name,"commit":{"sha":sha,"url":format!("{origin}/api/v3/repos/{}/{}/commits/{sha}",f.login,repo.name)},"protected":false}) }
fn ref_json(f:&Forge,repo:&Repo,reference:&str,sha:&str,origin:&str)->Value {
    let kind=repo.get(sha).map(|obj|match obj.object_type {store::ObjectType::Commit=>"commit",store::ObjectType::Tree=>"tree",store::ObjectType::Blob=>"blob",store::ObjectType::Tag=>"tag"}).unwrap_or("commit");
    json!({"ref":reference,"node_id":node_id("REF",&format!("{}:{reference}",repo.id)),"url":format!("{origin}/api/v3/repos/{}/{}/git/{reference}",f.login,repo.name),"object":{"sha":sha,"type":kind,"url":format!("{origin}/api/v3/repos/{}/{}/git/{}s/{sha}",f.login,repo.name,kind)}})
}

fn ref_list(f:&Forge,req:&Request,path:&str,origin:&str,repo:&Repo,prefix:&str)->Response {
    let mut refs:Vec<_>=repo.refs.iter().filter(|(name,_)|name.starts_with(prefix)).collect(); refs.sort_by(|a,b|a.0.cmp(b.0));
    paged(req,path,origin,refs.into_iter().map(|(name,sha)|ref_json(f,repo,name,sha,origin)).collect())
}

fn node_id(prefix:&str,key:&str)->String { format!("{prefix}_{}",base64_url(format!("gitforge:{prefix}:{key}").as_bytes())) }
fn base64_url(input:&[u8])->String { base64_encode(input).trim_end_matches('=').replace('+',"-").replace('/',"_") }

fn commit_json(f:&Forge,repo:&Repo,oid:&str,origin:&str)->Result<Value,ApiError> {
    let commit=f.commit_of(repo,oid).ok_or_else(not_found)?; let author=git::parse_signature(&commit.author); let committer=git::parse_signature(&commit.committer); let api=format!("{origin}/api/v3");
    let person=|sig:git::Signature|json!({"name":sig.name,"email":sig.email,"date":iso_timestamp(sig.time)});
    Ok(json!({"sha":oid,"node_id":node_id("C",oid),"url":format!("{api}/repos/{}/{}/commits/{oid}",f.login,repo.name),"html_url":format!("{origin}/{}/{}/commit/{oid}",f.login,repo.name),"commit":{"url":format!("{api}/repos/{}/{}/git/commits/{oid}",f.login,repo.name),"author":person(author),"committer":person(committer),"message":commit.message.trim_end_matches('\n'),"tree":{"sha":commit.tree,"url":format!("{api}/repos/{}/{}/git/trees/{}",f.login,repo.name,commit.tree)},"comment_count":0},"author":null,"committer":null,"parents":commit.parents.iter().map(|sha|json!({"sha":sha,"url":format!("{api}/repos/{}/{}/commits/{sha}",f.login,repo.name)})).collect::<Vec<_>>()}))
}

fn iso_timestamp(seconds:i64)->String {
    let days=seconds.div_euclid(86400); let day_seconds=seconds.rem_euclid(86400); let z=days+719468; let era=if z>=0 {z} else {z-146096}/146097; let doe=z-era*146097; let yoe=(doe-doe/1460+doe/36524-doe/146096)/365; let mut year=yoe+era*400; let doy=doe-(365*yoe+yoe/4-yoe/100); let mp=(5*doy+2)/153; let day=doy-(153*mp+2)/5+1; let month=mp+if mp<10 {3} else {-9}; year+=i64::from(month<=2);
    format!("{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",day_seconds/3600,(day_seconds%3600)/60,day_seconds%60)
}

fn tree_walk(f:&Forge,repo:&Repo,tree_oid:&str,prefix:&str,recursive:bool,origin:&str,out:&mut Vec<Value>)->Result<(),ApiError> {
    let entries=f.tree_entries(repo,tree_oid).map_err(|e|ApiError::new(404,e))?;
    for entry in entries {
        let path=if prefix.is_empty(){entry.name.clone()}else{format!("{prefix}/{}",entry.name)}; let is_tree=entry.mode=="40000";
        out.push(json!({"path":path,"mode":if is_tree {"040000"} else {&entry.mode},"type":if is_tree {"tree"} else if entry.mode=="160000" {"commit"} else {"blob"},"sha":entry.oid,"size":if is_tree {None} else {repo.get(&entry.oid).map(|obj|obj.data.len())},"url":format!("{origin}/api/v3/repos/{}/{}/git/{}s/{}",f.login,repo.name,if is_tree {"tree"} else {"blob"},entry.oid)}));
        if recursive&&is_tree {tree_walk(f,repo,&entry.oid,&path,true,origin,out)?;}
    }
    Ok(())
}

fn contents_read(f:&Forge,req:&Request,repo:&Repo,path:&str,origin:&str)->Result<Response,ApiError> {
    if repo.refs.is_empty(){return Err(ApiError::new(404,"This repository is empty."));}
    let reference=query(req,"ref").unwrap_or_else(||repo.default_branch.clone()); let commit=f.resolve(repo,&reference).ok_or_else(||ApiError::new(404,format!("No commit found for the ref {reference}")))?;
    match f.lookup_path(repo,&commit,path).map_err(|e|ApiError::new(404,e))?.ok_or_else(not_found)? {
        store::PathEntry::File{oid,..}=>Ok(json_response(200,content_file(f,repo,path,&oid,true,origin))),
        store::PathEntry::Directory{entries,..}=>Ok(json_response(200,Value::Array(entries.iter().map(|entry|content_entry(f,repo,if path.is_empty(){entry.name.clone()}else{format!("{path}/{}",entry.name)},&entry.mode,&entry.oid,origin)).collect()))),
    }
}

fn content_entry(f:&Forge,repo:&Repo,path:String,mode:&str,oid:&str,origin:&str)->Value {
    if mode!="40000" {let mut file=content_file(f,repo,&path,oid,false,origin); file["type"]=json!(if mode=="120000"{"symlink"}else if mode=="160000"{"submodule"}else{"file"}); return file;}
    let url=format!("{origin}/api/v3/repos/{}/{}/contents/{}",f.login,repo.name,encode_path(&path));
    json!({"type":"dir","name":path.rsplit('/').next().unwrap_or(&path),"path":path,"sha":oid,"size":0,"url":url,"html_url":format!("{}/{}/{}/tree/{}/{}",origin,f.login,repo.name,repo.default_branch,encode_path(&path)),"git_url":format!("{origin}/api/v3/repos/{}/{}/git/trees/{oid}",f.login,repo.name),"download_url":null,"_links":{"self":url}})
}

fn content_file(_f:&Forge,repo:&Repo,path:&str,oid:&str,with_content:bool,origin:&str)->Value {
    let url=format!("{origin}/api/v3/repos/{}/{}/contents/{}",_f.login,repo.name,encode_path(path)); let name=path.rsplit('/').next().unwrap_or(path); let size=repo.get(oid).map(|blob|blob.data.len()).unwrap_or(0);
    let html=format!("{origin}/{}/{}/blob/{}/{}",_f.login,repo.name,repo.default_branch,encode_path(path)); let git_url=format!("{origin}/api/v3/repos/{}/{}/git/blobs/{oid}",_f.login,repo.name);
    let mut result=json!({"type":"file","name":name,"path":path,"sha":oid,"size":size,"url":url,"html_url":html,"git_url":git_url,"download_url":format!("{origin}/{}/{}/raw/{}/{}",_f.login,repo.name,repo.default_branch,encode_path(path)),"_links":{"self":url,"git":git_url,"html":html}});
    if with_content { let content=repo.get(oid).map(|blob|base64_encode(&blob.data)).unwrap_or_default(); result["encoding"]=json!("base64"); result["content"]=json!(content.as_bytes().chunks(60).map(|x|String::from_utf8_lossy(x)).collect::<Vec<_>>().join("\n")); }
    result
}

fn content_write(f:&Forge,repo:&Repo,path:Option<&str>,blob:Option<&str>,commit:&str,origin:&str)->Result<Value,ApiError> {
    let full=commit_json(f,repo,commit,origin)?; let inner=&full["commit"];
    Ok(json!({"content":match (path,blob){(Some(path),Some(blob))=>content_file(f,repo,path,blob,true,origin),_=>Value::Null},"commit":{"sha":commit,"node_id":full["node_id"],"url":format!("{origin}/api/v3/repos/{}/{}/git/commits/{commit}",f.login,repo.name),"html_url":full["html_url"],"author":inner["author"],"committer":inner["committer"],"message":inner["message"],"tree":inner["tree"],"parents":full["parents"]}}))
}

fn path_entry_oid(entry:store::PathEntry)->String { match entry {store::PathEntry::File{oid,..}|store::PathEntry::Directory{oid,..}=>oid} }
fn changed_files(before:&BTreeMap<String,store::FileEntry>,after:&BTreeMap<String,store::FileEntry>)->Vec<Value> {
    let paths:std::collections::BTreeSet<String>=before.keys().chain(after.keys()).cloned().collect();
    paths.into_iter().filter_map(|path|{let b=before.get(&path);let a=after.get(&path);if b==a{return None;}Some(json!({"filename":path,"status":if b.is_none(){"added"}else if a.is_none(){"removed"}else{"modified"},"sha":a.or(b).map(|entry|entry.oid.as_str()).unwrap_or("")}))}).collect()
}

fn pull_json(f:&mut Forge,repo:&Repo,pull:&Pull,origin:&str,full:bool)->Result<Value,ApiError> {
    let (head_sha,base_sha)=f.pull_tips(repo,pull); let api=format!("{origin}/api/v3");
    let side=|reference:&str,sha:&str|json!({"label":format!("{}:{reference}",f.login),"ref":reference,"sha":sha,"user":user_json(f,origin),"repo":repo_json(f,repo,origin)});
    let mut result=json!({"id":pull.id,"node_id":node_id("PR",&pull.id.to_string()),"url":format!("{api}/repos/{}/{}/pulls/{}",f.login,repo.name,pull.number),"html_url":format!("{}/{}/{}/pull/{}",origin,f.login,repo.name,pull.number),"diff_url":format!("{}/{}/{}/pull/{}.diff",origin,f.login,repo.name,pull.number),"issue_url":format!("{api}/repos/{}/{}/issues/{}",f.login,repo.name,pull.number),"number":pull.number,"state":pull.state,"locked":false,"title":pull.title,"body":pull.body,"user":user_json(f,origin),"labels":[],"created_at":pull.created_at,"updated_at":pull.updated_at,"closed_at":pull.closed_at,"merged_at":pull.merged_at,"merge_commit_sha":pull.merge_commit_sha,"draft":false,"head":side(&pull.head,&head_sha),"base":side(&pull.base,&base_sha),"author_association":"OWNER","merged":pull.merged});
    if full {let mergeable=f.mergeable(repo,pull);let files=f.pull_files(repo,pull).map_err(|e|ApiError::new(500,e))?;let additions:usize=files.iter().map(|file|file.additions).sum();let deletions:usize=files.iter().map(|file|file.deletions).sum();result["mergeable"]=json!(mergeable);result["mergeable_state"]=json!(if mergeable{"clean"}else{"dirty"});result["merged_by"]=if pull.merged {user_json(f,origin)} else {Value::Null};result["comments"]=json!(pull.comments.len());result["review_comments"]=json!(0);result["commits"]=json!(f.pull_commits(repo,pull).len());result["additions"]=json!(additions);result["deletions"]=json!(deletions);result["changed_files"]=json!(files.len());}
    Ok(result)
}

fn pull_file_json(file:&FileDiff,repo:&Repo,origin:&str,login:&str,api:&str)->Value {
    json!({"sha":file.sha,"filename":file.path,"status":file.status,"additions":file.additions,"deletions":file.deletions,"changes":file.additions+file.deletions,"blob_url":format!("{origin}/{login}/{}/blob/{}/{}",repo.name,file.sha,encode_path(&file.path)),"raw_url":format!("{origin}/{login}/{}/raw/{}/{}",repo.name,file.sha,encode_path(&file.path)),"contents_url":format!("{api}/repos/{login}/{}/contents/{}",repo.name,encode_path(&file.path)),"patch":file.patch})
}

fn base64_encode(input:&[u8])->String {
    const TABLE:&[u8;64]=b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/"; let mut out=String::with_capacity((input.len()+2)/3*4);
    for chunk in input.chunks(3) {let a=chunk[0];let b=*chunk.get(1).unwrap_or(&0);let c=*chunk.get(2).unwrap_or(&0);out.push(TABLE[(a>>2)as usize]as char);out.push(TABLE[(((a&3)<<4)|(b>>4))as usize]as char);if chunk.len()>1 {out.push(TABLE[(((b&15)<<2)|(c>>6))as usize]as char);}else{out.push('=');}if chunk.len()>2 {out.push(TABLE[(c&63)as usize]as char);}else{out.push('=');}}
    out
}
fn base64_decode(input:&str)->Option<Vec<u8>> {
    let mut out=Vec::new();let mut accum=0u32;let mut bits=0u8;
    for byte in input.bytes().filter(|b|!b.is_ascii_whitespace()) {if byte==b'='{break;} let value=match byte {b'A'..=b'Z'=>byte-b'A',b'a'..=b'z'=>byte-b'a'+26,b'0'..=b'9'=>byte-b'0'+52,b'+'=>62,b'/'=>63,_=>return None}as u32;accum=(accum<<6)|value;bits+=6;if bits>=8{bits-=8;out.push(((accum>>bits)&255)as u8);}}
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test] fn wildcard_routes_keep_nested_tail(){assert_eq!(route("GET","GET","/repos/:owner/:repo/contents/*path","/repos/a/b/contents/src/one.txt").unwrap(),vec!["a","b","src/one.txt"]);}
    #[test] fn base64_round_trip(){let source=b"hello\nworld";assert_eq!(base64_decode(&base64_encode(source)).unwrap(),source);}
}
