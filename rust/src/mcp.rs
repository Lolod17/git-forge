//! Streamable HTTP JSON-RPC endpoint, backed by the same Forge used by REST.
use crate::http::{Request, Response};
use crate::store::{self, Forge, IssueFilter, IssueUpdate, MergeOptions};
use serde_json::{json, Map, Value};

const REVISIONS: [&str; 5] = ["2026-07-28", "2025-11-25", "2025-06-18", "2025-03-26", "2024-11-05"];
const SERVER_INFO: &str = "{\"name\":\"gitforge\",\"title\":\"gitforge — a GitHub-compatible forge\",\"version\":\"0.1.0\"}";

#[derive(Clone)]
struct Tool {
    name: &'static str,
    description: &'static str,
    schema: Value,
}

fn tools() -> Vec<Tool> {
    let owner = json!({"type":"string","description":"Repository owner (the authenticated user's login)"});
    let repo = json!({"type":"string","description":"Repository name"});
    let string = |description: &str| json!({"type":"string","description":description});
    let number = |description: &str| json!({"type":"number","description":description});
    vec![
        Tool { name:"create_repo", description:"Create a new repository owned by the authenticated user.", schema:json!({"type":"object","properties":{"name":string("Repository name"),"description":string("Repository description"),"private":{"type":"boolean","description":"Whether the repository is private"},"autoInit":{"type":"boolean","description":"Initialize with a README"}},"required":["name"]}) },
        Tool { name:"get_repo", description:"Get a repository by owner and name.", schema:json!({"type":"object","properties":{"owner":owner,"repo":repo},"required":["owner","repo"]}) },
        Tool { name:"list_repos", description:"List the repositories of the authenticated user.", schema:json!({"type":"object","properties":{"perPage":{"type":"number","description":"Results per page (max 100)"},"page":{"type":"number","description":"Page number"}}}) },
        Tool { name:"create_issue", description:"Create a new issue in a repository.", schema:json!({"type":"object","properties":{"owner":owner,"repo":repo,"title":string("Issue title"),"body":string("Issue body (Markdown)"),"labels":{"type":"array","items":{"type":"string"},"description":"Label names (each must exist)"}},"required":["owner","repo","title"]}) },
        Tool { name:"get_issue", description:"Get one issue of a repository by number.", schema:json!({"type":"object","properties":{"owner":owner,"repo":repo,"issue_number":number("Issue number")},"required":["owner","repo","issue_number"]}) },
        Tool { name:"list_issues", description:"List the issues of a repository, newest first.", schema:json!({"type":"object","properties":{"owner":owner,"repo":repo,"state":{"type":"string","enum":["open","closed","all"],"description":"Filter by state (default open)"},"labels":{"type":"array","items":{"type":"string"},"description":"Only issues carrying all these labels"}},"required":["owner","repo"]}) },
        Tool { name:"update_issue", description:"Update an issue: title, body, state (open or closed) or labels.", schema:json!({"type":"object","properties":{"owner":owner,"repo":repo,"issue_number":number("Issue number"),"title":string("New title"),"body":string("New body"),"state":{"type":"string","enum":["open","closed"],"description":"New state"}},"required":["owner","repo","issue_number"]}) },
        Tool { name:"add_issue_comment", description:"Add a comment to an issue or pull request.", schema:json!({"type":"object","properties":{"owner":owner,"repo":repo,"issue_number":number("Issue or pull request number"),"body":string("Comment body")},"required":["owner","repo","issue_number","body"]}) },
        Tool { name:"create_branch", description:"Create a branch from another branch (default: the repository's default branch).", schema:json!({"type":"object","properties":{"owner":owner,"repo":repo,"branch":string("New branch name"),"from_branch":string("Source branch")},"required":["owner","repo","branch"]}) },
        Tool { name:"create_pull_request", description:"Open a pull request from a head branch into a base branch.", schema:json!({"type":"object","properties":{"owner":owner,"repo":repo,"title":string("Pull request title"),"body":string("Pull request description"),"head":string("Branch with the changes"),"base":string("Branch to merge into")},"required":["owner","repo","title","head","base"]}) },
        Tool { name:"get_pull_request", description:"Get one pull request by number.", schema:json!({"type":"object","properties":{"owner":owner,"repo":repo,"pull_number":number("Pull request number")},"required":["owner","repo","pull_number"]}) },
        Tool { name:"list_pull_requests", description:"List the pull requests of a repository.", schema:json!({"type":"object","properties":{"owner":owner,"repo":repo,"state":{"type":"string","enum":["open","closed","all"],"description":"Filter by state (default open)"}},"required":["owner","repo"]}) },
        Tool { name:"merge_pull_request", description:"Merge a pull request (merge, squash or rebase).", schema:json!({"type":"object","properties":{"owner":owner,"repo":repo,"pull_number":number("Pull request number"),"merge_method":{"type":"string","enum":["merge","squash","rebase"],"description":"Merge method (default merge)"},"commit_title":string("Title of the merge commit"),"commit_message":string("Extra detail for the merge commit")},"required":["owner","repo","pull_number"]}) },
        Tool { name:"get_file_contents", description:"Get a file (base64 and decoded text) or a directory listing at a path and ref.", schema:json!({"type":"object","properties":{"owner":owner,"repo":repo,"path":string("Path in the repository (empty for the root)"),"ref":string("Branch, tag or commit (default branch when omitted)")},"required":["owner","repo"]}) },
        Tool { name:"create_or_update_file", description:"Create or update one file on a branch with a commit (pass the current blob sha to update).", schema:json!({"type":"object","properties":{"owner":owner,"repo":repo,"path":string("Path of the file"),"content":string("New file content (plain text)"),"message":string("Commit message"),"branch":string("Branch (default branch when omitted)"),"sha":string("Current blob sha, required when the file exists")},"required":["owner","repo","path","content","message"]}) },
    ]
}

fn rpc_result(id: &Value, result: Value) -> Value { json!({"jsonrpc":"2.0","id":id,"result":result}) }
fn rpc_error(id: &Value, code: i64, message: impl AsRef<str>) -> Value {
    json!({"jsonrpc":"2.0","id":id,"error":{"code":code,"message":message.as_ref()}})
}
fn response(status: u16, value: &Value) -> Response {
    Response::send_json_bytes(status, serde_json::to_vec(value).unwrap_or_else(|_| b"{}".to_vec()))
}
fn id_of(request: &Map<String, Value>) -> (bool, Value) {
    match request.get("id") {
        Some(Value::String(_) | Value::Number(_)) => (true, request["id"].clone()),
        _ => (false, Value::Null),
    }
}
fn args_object(value: Option<&Value>) -> Map<String, Value> {
    value.and_then(Value::as_object).cloned().unwrap_or_default()
}
fn string_arg(args: &Map<String, Value>, key: &str) -> Option<String> {
    args.get(key).and_then(|value| match value {
        Value::String(text) if !text.is_empty() => Some(text.clone()),
        Value::Number(number) => Some(number.to_string()),
        _ => None,
    })
}
fn required(args: &Map<String, Value>, key: &str) -> Result<String, String> {
    string_arg(args, key).ok_or_else(|| format!("missing required parameter: {key}"))
}
fn number_arg(args: &Map<String, Value>, key: &str) -> Result<u64, String> {
    let value = args.get(key).and_then(|value| match value {
        Value::Number(number) => number.as_u64(),
        Value::String(text) => text.parse::<u64>().ok(),
        _ => None,
    }).filter(|value| *value > 0);
    value.ok_or_else(|| format!("missing required parameter: {key} (a positive integer)"))
}
fn json_from_store(text: String) -> Value { serde_json::from_str(&text).unwrap_or(Value::Null) }
fn repo_for(forge: &Forge, args: &Map<String, Value>) -> Result<store::Repo, String> {
    let owner = required(args, "owner")?;
    let name = required(args, "repo")?;
    forge.repo_by_name(&owner, &name).ok_or_else(|| format!("repository {owner}/{name} not found (HTTP 404)"))
}
fn error_text(error: store::ApiError) -> String {
    if error.errors.is_empty() { format!("{} (HTTP {})", error.message, error.status) }
    else {
        let errors = serde_json::to_string(&error.errors.iter().map(|e| json!({"resource":e.resource,"code":e.code,"field":e.field,"message":e.message,"value":e.value})).collect::<Vec<_>>()).unwrap_or_default();
        format!("{} {} (HTTP {})", error.message, errors, error.status)
    }
}
fn labels(args: &Map<String, Value>) -> Vec<String> {
    args.get("labels").and_then(Value::as_array).map(|items| items.iter().filter_map(Value::as_str).map(str::to_string).collect()).unwrap_or_default()
}

fn run_tool(forge: &mut Forge, name: &str, args: &Map<String, Value>) -> Result<Value, String> {
    match name {
        "create_repo" => {
            let name = required(args, "name")?;
            let description = args.get("description").and_then(Value::as_str).map(str::to_string);
            let private = args.get("private").is_some_and(|v| v == true || v == "true");
            let mut created = forge.create_repo(name, description, private, true).map_err(error_text)?;
            if args.get("autoInit") == Some(&Value::Bool(true)) {
                let contents = format!("# {}\n", created.name);
                forge.contents_commit(&mut created, "README.md", contents.as_bytes(), "Initial commit", None, None).map_err(error_text)?;
            }
            Ok(json_from_store(forge.repo_json(&created)))
        }
        "get_repo" => { let repo = repo_for(forge, args)?; Ok(json_from_store(forge.repo_json(&repo))) }
        "list_repos" => {
            let per_page = args.get("perPage").and_then(Value::as_u64).unwrap_or(30).clamp(1,100) as usize;
            let page = args.get("page").and_then(Value::as_u64).unwrap_or(1).max(1) as usize;
            let values = forge.list_repos().into_iter().skip(page.saturating_sub(1).saturating_mul(per_page)).take(per_page).map(|repo| json_from_store(forge.repo_json(&repo))).collect();
            Ok(Value::Array(values))
        }
        "create_issue" => {
            let mut repo = repo_for(forge, args)?;
            let title = required(args, "title")?;
            let body = args.get("body").and_then(Value::as_str).unwrap_or("");
            let issue = forge.create_issue(&mut repo, &title, body, &labels(args)).map_err(error_text)?;
            Ok(json_from_store(forge.issue_json(&repo, &issue)))
        }
        "get_issue" => {
            let repo = repo_for(forge, args)?;
            let number = number_arg(args, "issue_number")?;
            let issue = repo.issues.get(&number).ok_or_else(|| "issue not found (HTTP 404)".to_string())?;
            Ok(json_from_store(forge.issue_json(&repo, issue)))
        }
        "list_issues" => {
            let repo = repo_for(forge, args)?;
            let state = args.get("state").and_then(Value::as_str).unwrap_or("open").to_ascii_lowercase();
            let issues = forge.list_issues(&repo, &IssueFilter { state:Some(state), labels:labels(args), ..IssueFilter::default() });
            Ok(Value::Array(issues.iter().map(|issue| json_from_store(forge.issue_json(&repo, issue))).collect()))
        }
        "update_issue" => {
            let mut repo = repo_for(forge, args)?;
            let number = number_arg(args, "issue_number")?;
            let patch = IssueUpdate { title:args.get("title").and_then(Value::as_str).map(str::to_string), body:args.get("body").and_then(Value::as_str).map(str::to_string), state:args.get("state").and_then(Value::as_str).map(str::to_string), ..IssueUpdate::default() };
            let issue = forge.update_issue(&mut repo, number, patch).map_err(error_text)?;
            Ok(json_from_store(forge.issue_json(&repo, &issue)))
        }
        "add_issue_comment" => {
            let mut repo = repo_for(forge, args)?;
            let number = number_arg(args, "issue_number")?;
            let body = required(args, "body")?;
            let comment = forge.add_comment(&mut repo, number, &body).map_err(error_text)?;
            Ok(json!({"id":comment.id,"body":comment.body,"created_at":comment.created_at,"updated_at":comment.updated_at,"issue_url":format!("/api/v3/repos/{}/{}/issues/{}",forge.login,repo.name,number)}))
        }
        "create_branch" => {
            let mut repo = repo_for(forge, args)?;
            let branch = required(args, "branch")?;
            let from = args.get("from_branch").and_then(Value::as_str).filter(|v| !v.is_empty()).unwrap_or(&repo.default_branch).to_string();
            let sha = repo.branch_tip(&from).cloned().ok_or_else(|| format!("branch {from} not found (HTTP 404)"))?;
            if repo.branch_tip(&branch).is_some() { return Err("Reference already exists (HTTP 422)".to_string()); }
            forge.write_ref(&mut repo, &format!("refs/heads/{branch}"), &sha);
            Ok(json!({"ref":format!("refs/heads/{branch}"),"object":{"sha":sha,"type":"commit"}}))
        }
        "create_pull_request" => {
            let mut repo = repo_for(forge, args)?;
            let title = required(args, "title")?; let head = required(args, "head")?; let base = required(args, "base")?;
            let body = args.get("body").and_then(Value::as_str).unwrap_or("");
            let pull = forge.create_pull(&mut repo, &title, body, &head, &base).map_err(error_text)?;
            Ok(json_from_store(forge.pull_json(&repo, &pull)))
        }
        "get_pull_request" => {
            let repo = repo_for(forge, args)?; let number = number_arg(args, "pull_number")?;
            let pull = repo.pulls.get(&number).ok_or_else(|| "pull request not found (HTTP 404)".to_string())?;
            Ok(json_from_store(forge.pull_json(&repo, pull)))
        }
        "list_pull_requests" => {
            let repo = repo_for(forge, args)?; let state = args.get("state").and_then(Value::as_str).unwrap_or("open").to_ascii_lowercase();
            let pulls = forge.list_pulls(&repo, Some(&state));
            Ok(Value::Array(pulls.iter().map(|pull| json_from_store(forge.pull_json(&repo, pull))).collect()))
        }
        "merge_pull_request" => {
            let mut repo = repo_for(forge, args)?; let number = number_arg(args, "pull_number")?;
            if !repo.pulls.contains_key(&number) { return Err("pull request not found (HTTP 404)".to_string()); }
            let options = MergeOptions { method:args.get("merge_method").and_then(Value::as_str).map(str::to_string), title:args.get("commit_title").and_then(Value::as_str).map(str::to_string), message:args.get("commit_message").and_then(Value::as_str).map(str::to_string), ..MergeOptions::default() };
            let sha = forge.merge_pull(&mut repo, number, options).map_err(error_text)?;
            Ok(json!({"sha":sha,"merged":true,"message":"Pull Request successfully merged","pull_number":number}))
        }
        "get_file_contents" => {
            let repo = repo_for(forge, args)?;
            let reference = args.get("ref").and_then(Value::as_str).filter(|s| !s.is_empty()).unwrap_or(&repo.default_branch);
            let commit = forge.resolve(&repo, reference).ok_or_else(|| format!("ref {reference} not found (HTTP 404)"))?;
            let path = args.get("path").and_then(Value::as_str).unwrap_or("").trim_matches('/');
            match forge.lookup_path(&repo, &commit, path).map_err(|e| format!("{e} (HTTP 404)"))? {
                None => Err(format!("{path} not found (HTTP 404)")),
                Some(store::PathEntry::Directory { entries, .. }) => Ok(Value::Array(entries.into_iter().map(|entry| json!({"name":entry.name,"path":if path.is_empty(){entry.name.clone()}else{format!("{path}/{}",entry.name)},"mode":entry.mode,"type":if entry.mode == "40000" {"dir"} else {"file"},"sha":entry.oid})).collect())),
                Some(store::PathEntry::File { mode, oid }) => {
                    let data = repo.get(&oid).map(|object| object.data.clone()).unwrap_or_default();
                    Ok(json!({"name":path.rsplit('/').next().unwrap_or(path),"path":path,"sha":oid,"size":data.len(),"type":"file","encoding":"base64","content":base64(&data),"text":String::from_utf8_lossy(&data),"mode":mode}))
                }
            }
        }
        "create_or_update_file" => {
            let mut repo = repo_for(forge, args)?; let path = required(args, "path")?;
            let content = args.get("content").and_then(Value::as_str).ok_or_else(|| "missing required parameter: content".to_string())?;
            let message = required(args, "message")?;
            let branch = args.get("branch").and_then(Value::as_str).filter(|s| !s.is_empty());
            let sha = args.get("sha").and_then(Value::as_str);
            let (commit, blob) = forge.contents_commit(&mut repo, &path, content.as_bytes(), &message, branch, sha).map_err(error_text)?;
            Ok(json!({"content":{"path":path,"sha":blob,"size":content.len()},"commit":{"sha":commit,"message":message}}))
        }
        _ => Err(format!("Unknown tool: {name}")),
    }
}

fn base64(bytes: &[u8]) -> String {
    const TABLE: &[u8;64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for chunk in bytes.chunks(3) {
        let a=chunk[0]; let b=*chunk.get(1).unwrap_or(&0); let c=*chunk.get(2).unwrap_or(&0);
        out.push(TABLE[(a >> 2) as usize] as char); out.push(TABLE[(((a & 3) << 4) | (b >> 4)) as usize] as char);
        out.push(if chunk.len()>1 {TABLE[(((b & 15)<<2)|(c>>6)) as usize] as char} else {'='});
        out.push(if chunk.len()>2 {TABLE[(c & 63) as usize] as char} else {'='});
    }
    out
}

fn one(forge: &mut Forge, message: &Value, origin: &str) -> (u16, Option<Value>, Vec<(String,String)>) {
    let Some(request) = message.as_object() else { return (400,Some(rpc_error(&Value::Null,-32600,"Invalid Request")),vec![]); };
    let (has_id,id) = id_of(request);
    let Some(method) = request.get("method").and_then(Value::as_str) else {
        if request.contains_key("result") || request.contains_key("error") { return (202,None,vec![]); }
        return (400,Some(rpc_error(&id,-32600,"Invalid Request: no method")),vec![]);
    };
    if !has_id { return (202,None,vec![]); }
    let params = args_object(request.get("params"));
    let value = match method {
        "server/discover" => {
            let info = json_from_store(SERVER_INFO.to_string());
            rpc_result(&id,json!({"supportedVersions":REVISIONS,"capabilities":{"tools":{"listChanged":false}},"serverInfo":info,"instructions":"A GitHub-compatible forge. Tools act on the authenticated user's repositories, issues and pull requests.","_meta":{"io.modelcontextprotocol/serverInfo":info}}))
        }
        "initialize" => {
            let asked=params.get("protocolVersion").and_then(Value::as_str).unwrap_or("");
            let version=if REVISIONS[1..].contains(&asked) {asked} else {"2025-11-25"};
            let session=format!("gitforge-session-{}",id);
            let info=json_from_store(SERVER_INFO.to_string());
            return (200,Some(rpc_result(&id,json!({"protocolVersion":version,"capabilities":{"tools":{"listChanged":false}},"serverInfo":info,"instructions":"A GitHub-compatible forge."}))),vec![("mcp-session-id".into(),session)]);
        }
        "ping" => rpc_result(&id,json!({})),
        "tools/list" => {
            let listed:Vec<Value>=tools().into_iter().map(|tool| json!({"name":tool.name,"description":tool.description,"inputSchema":tool.schema})).collect();
            rpc_result(&id,json!({"tools":listed}))
        }
        "tools/call" => {
            let name=params.get("name").and_then(Value::as_str).unwrap_or("");
            let defs=tools();
            let Some(tool)=defs.iter().find(|tool| tool.name==name) else {
                return (200,Some(rpc_error(&id,-32602,if name.is_empty(){"Unknown tool: (no name)".to_string()}else{format!("Unknown tool: {name}")})),vec![]);
            };
            let args=args_object(params.get("arguments"));
            let missing:Vec<&str>=tool.schema.get("required").and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str).filter(|key| args.get(*key).is_none_or(Value::is_null) || args.get(*key)==Some(&Value::String(String::new()))).collect();
            if !missing.is_empty() {
                let message=format!("missing required parameter{}: {}",if missing.len()>1{"s"}else{""},missing.join(", "));
                rpc_result(&id,json!({"content":[{"type":"text","text":message}],"isError":true}))
            } else {
                match run_tool(forge,name,&args) {
                    Ok(result) => {
                        let text=serde_json::to_string(&result).unwrap_or_else(|_|"null".to_string());
                        let structured=if result.is_array(){json!({"items":result})}else{result.clone()};
                        rpc_result(&id,json!({"content":[{"type":"text","text":text}],"structuredContent":structured,"isError":false}))
                    }
                    Err(error) => rpc_result(&id,json!({"content":[{"type":"text","text":error}],"isError":true})),
                }
            }
        }
        "resources/list" => rpc_result(&id,json!({"resources":[]})),
        "prompts/list" => rpc_result(&id,json!({"prompts":[]})),
        _ => rpc_error(&id,-32601,format!("Method not found: {method}")),
    };
    let _ = origin;
    (200,Some(value),vec![])
}

/// Handle one POST body. `origin` is provided for compatibility with the other HTTP surfaces.
pub fn handle(forge: &mut Forge, request: &Request, origin: &str) -> Response {
    let parsed: Value = match serde_json::from_slice(&request.body) {
        Ok(value) => value,
        Err(_) => return response(400,&rpc_error(&Value::Null,-32700,"Parse error: the request body is not valid JSON")),
    };
    if let Some(batch)=parsed.as_array() {
        if batch.is_empty() { return response(400,&rpc_error(&Value::Null,-32600,"Invalid Request: empty batch")); }
        let mut answers=Vec::new(); let mut extras=Vec::new();
        for entry in batch {
            let (_status,answer,headers)=one(forge,entry,origin);
            if let Some(answer)=answer { answers.push(answer); }
            extras.extend(headers);
        }
        if answers.is_empty() { let mut result=Response::send_json_bytes(202,Vec::new()); result.extra=extras; return result; }
        let mut result=response(200,&Value::Array(answers)); result.extra=extras; result
    } else {
        let (status,answer,headers)=one(forge,&parsed,origin);
        let mut result=match answer { Some(value)=>response(status,&value), None=>Response::send_json_bytes(202,Vec::new()) };
        result.extra=headers;
        result
    }
}
