//! A small, deliberately schema-driven GraphQL executor for the GitHub-shaped surface.
//! The parser accepts the document features used by githubv4/gh (variables, aliases,
//! fragments, directives and batched requests) and resolvers operate on the shared Forge.

use crate::http::{Request, Response};
use crate::store::{self, ApiError, Forge, Issue, Pull, Repo};
use serde_json::{json, Map, Value};
use std::collections::{HashMap, HashSet};

#[derive(Clone, Debug)]
struct Token {
    kind: &'static str,
    text: String,
    line: usize,
    col: usize,
}
#[derive(Debug)]
struct Syntax {
    message: String,
    line: usize,
    col: usize,
}

fn lex(src: &str) -> Result<Vec<Token>, Syntax> {
    let chars: Vec<char> = src.chars().collect();
    let (mut p, mut line, mut line_start) = (0usize, 1usize, 0usize);
    let mut out = Vec::new();
    while p < chars.len() {
        let c = chars[p];
        if c == '\n' {
            p += 1;
            line += 1;
            line_start = p;
            continue;
        }
        if c == ' ' || c == '\t' || c == '\r' || c == ',' || c == '\u{feff}' {
            p += 1;
            continue;
        }
        if c == '#' {
            while p < chars.len() && chars[p] != '\n' {
                p += 1;
            }
            continue;
        }
        let (l, col) = (line, p - line_start + 1);
        if p + 2 < chars.len() && chars[p..p + 3] == ['.', '.', '.'] {
            out.push(Token {
                kind: "punct",
                text: "...".into(),
                line: l,
                col,
            });
            p += 3;
            continue;
        }
        if "!$&()):=@[]{}|".contains(c) {
            out.push(Token {
                kind: "punct",
                text: c.to_string(),
                line: l,
                col,
            });
            p += 1;
            continue;
        }
        if c.is_ascii_alphabetic() || c == '_' {
            let start = p;
            p += 1;
            while p < chars.len() && (chars[p].is_ascii_alphanumeric() || chars[p] == '_') {
                p += 1;
            }
            out.push(Token {
                kind: "name",
                text: chars[start..p].iter().collect(),
                line: l,
                col,
            });
            continue;
        }
        if c == '-' || c.is_ascii_digit() {
            let start = p;
            p += 1;
            while p < chars.len()
                && (chars[p].is_ascii_digit()
                    || chars[p] == '.'
                    || chars[p] == 'e'
                    || chars[p] == 'E'
                    || chars[p] == '+'
                    || chars[p] == '-')
            {
                p += 1;
            }
            let text: String = chars[start..p].iter().collect();
            let float = text.contains('.') || text.contains('e') || text.contains('E');
            if text.parse::<f64>().is_err() {
                return Err(Syntax {
                    message: format!("Syntax Error: Invalid number, unexpected {c:?}."),
                    line: l,
                    col,
                });
            }
            out.push(Token {
                kind: if float { "float" } else { "int" },
                text,
                line: l,
                col,
            });
            continue;
        }
        if c == '"' {
            if p + 2 < chars.len() && chars[p..p + 3] == ['"', '"', '"'] {
                let start = p + 3;
                p += 3;
                let mut end = None;
                while p + 2 < chars.len() {
                    if chars[p..p + 3] == ['"', '"', '"'] {
                        end = Some(p);
                        break;
                    }
                    if chars[p] == '\n' {
                        line += 1;
                        line_start = p + 1;
                    }
                    p += 1;
                }
                let Some(e) = end else {
                    return Err(Syntax {
                        message: "Syntax Error: Unterminated string.".into(),
                        line: l,
                        col,
                    });
                };
                let text: String = chars[start..e].iter().collect();
                p = e + 3;
                out.push(Token {
                    kind: "string",
                    text: text.replace("\\\"\"\"", "\"\"\""),
                    line: l,
                    col,
                });
                continue;
            }
            p += 1;
            let mut value = String::new();
            let mut closed = false;
            while p < chars.len() {
                let ch = chars[p];
                if ch == '"' {
                    p += 1;
                    closed = true;
                    break;
                }
                if ch == '\n' {
                    break;
                }
                if ch == '\\' {
                    p += 1;
                    if p >= chars.len() {
                        break;
                    }
                    let esc = chars[p];
                    match esc {
                        '"' => value.push('"'),
                        '\\' => value.push('\\'),
                        '/' => value.push('/'),
                        'b' => value.push('\u{8}'),
                        'f' => value.push('\u{c}'),
                        'n' => value.push('\n'),
                        'r' => value.push('\r'),
                        't' => value.push('\t'),
                        'u' => {
                            if p + 4 >= chars.len() {
                                return Err(Syntax {
                                    message: "Syntax Error: Invalid character escape sequence."
                                        .into(),
                                    line: l,
                                    col,
                                });
                            }
                            let hex: String = chars[p + 1..p + 5].iter().collect();
                            if let Ok(code) = u16::from_str_radix(&hex, 16) {
                                if let Some(ch) = char::from_u32(code as u32) {
                                    value.push(ch);
                                }
                            } else {
                                return Err(Syntax {
                                    message: "Syntax Error: Invalid character escape sequence."
                                        .into(),
                                    line: l,
                                    col,
                                });
                            }
                            p += 4;
                        }
                        _ => {
                            return Err(Syntax {
                                message: "Syntax Error: Invalid character escape sequence.".into(),
                                line: l,
                                col,
                            })
                        }
                    }
                    p += 1;
                    continue;
                }
                value.push(ch);
                p += 1;
            }
            if !closed {
                return Err(Syntax {
                    message: "Syntax Error: Unterminated string.".into(),
                    line: l,
                    col,
                });
            }
            out.push(Token {
                kind: "string",
                text: value,
                line: l,
                col,
            });
            continue;
        }
        return Err(Syntax {
            message: format!("Syntax Error: Unexpected character: {c:?}."),
            line: l,
            col,
        });
    }
    out.push(Token {
        kind: "eof",
        text: String::new(),
        line,
        col: chars.len().saturating_sub(line_start) + 1,
    });
    Ok(out)
}

#[derive(Clone, Debug)]
enum Val {
    Var(String),
    Atom(String, String),
    Null,
    List(Vec<Val>),
    Obj(Vec<(String, Val)>),
}
#[derive(Clone, Debug)]
struct Arg {
    name: String,
    value: Val,
}
#[derive(Clone, Debug)]
struct Dir {
    name: String,
    args: Vec<Arg>,
}
#[derive(Clone, Debug)]
struct Field {
    name: String,
    alias: Option<String>,
    args: Vec<Arg>,
    dirs: Vec<Dir>,
    sels: Option<Vec<Sel>>,
    line: usize,
    col: usize,
}
#[derive(Clone, Debug)]
enum Sel {
    Field(Field),
    Spread(String, Vec<Dir>, usize, usize),
    Inline(Option<String>, Vec<Dir>, Vec<Sel>),
}
#[derive(Clone, Debug)]
struct VarDef {
    name: String,
    ty: String,
    default: Option<Val>,
}
#[derive(Clone, Debug)]
struct Op {
    kind: String,
    name: Option<String>,
    vars: Vec<VarDef>,
    sels: Vec<Sel>,
}
#[derive(Clone, Debug)]
struct Fragment {
    condition: String,
    sels: Vec<Sel>,
}
#[derive(Clone, Debug)]
enum Def {
    Op(Op),
    Fragment(String, Fragment),
}

struct Parser {
    ts: Vec<Token>,
    p: usize,
}
impl Parser {
    fn new(src: &str) -> Result<Self, Syntax> {
        Ok(Self {
            ts: lex(src)?,
            p: 0,
        })
    }
    fn peek(&self) -> &Token {
        &self.ts[self.p.min(self.ts.len().saturating_sub(1))]
    }
    fn next(&mut self) -> Token {
        let t = self.peek().clone();
        self.p = self
            .p
            .saturating_add(1)
            .min(self.ts.len().saturating_sub(1));
        t
    }
    fn fail<T>(&self, expected: &str) -> Result<T, Syntax> {
        let t = self.peek();
        let found = if t.kind == "eof" {
            "<EOF>".into()
        } else {
            format!("{:?}", t.text)
        };
        Err(Syntax {
            message: format!("Syntax Error: Expected {expected}, found {found}."),
            line: t.line,
            col: t.col,
        })
    }
    fn is(&self, s: &str) -> bool {
        self.peek().kind == "punct" && self.peek().text == s
    }
    fn expect(&mut self, s: &str) -> Result<Token, Syntax> {
        let t = self.next();
        if t.kind != "punct" || t.text != s {
            let found = if t.kind == "eof" {
                "<EOF>".into()
            } else {
                format!("{:?}", t.text)
            };
            return Err(Syntax {
                message: format!("Syntax Error: Expected {s:?}, found {found}."),
                line: t.line,
                col: t.col,
            });
        }
        Ok(t)
    }
    fn name(&mut self) -> Result<String, Syntax> {
        let t = self.next();
        if t.kind != "name" {
            return self.fail("Name");
        }
        Ok(t.text)
    }
    fn document(&mut self) -> Result<Vec<Def>, Syntax> {
        let mut defs = Vec::new();
        while self.peek().kind != "eof" {
            defs.push(self.definition()?);
        }
        if defs.is_empty() {
            return self.fail("a definition");
        }
        Ok(defs)
    }
    fn definition(&mut self) -> Result<Def, Syntax> {
        if self.is("{") {
            return Ok(Def::Op(Op {
                kind: "query".into(),
                name: None,
                vars: vec![],
                sels: self.selection_set()?,
            }));
        }
        if self.peek().kind == "name"
            && ["query", "mutation", "subscription"].contains(&self.peek().text.as_str())
        {
            let kind = self.next().text;
            let name = if self.peek().kind == "name" {
                Some(self.name()?)
            } else {
                None
            };
            let mut vars = vec![];
            if self.is("(") {
                self.next();
                while !self.is(")") {
                    self.expect("$")?;
                    let vn = self.name()?;
                    self.expect(":")?;
                    let ty = self.type_ref()?;
                    let default = if self.is("=") {
                        self.next();
                        Some(self.value(true)?)
                    } else {
                        None
                    };
                    let _ = self.directives()?;
                    vars.push(VarDef {
                        name: vn,
                        ty,
                        default,
                    });
                    if self.peek().kind == "eof" {
                        return self.fail("\")\"");
                    }
                }
                self.expect(")")?;
            }
            let _ = self.directives()?;
            return Ok(Def::Op(Op {
                kind,
                name,
                vars,
                sels: self.selection_set()?,
            }));
        }
        if self.peek().kind == "name" && self.peek().text == "fragment" {
            self.next();
            let name = self.name()?;
            if self.name()? != "on" {
                return self.fail("\"on\"");
            }
            let condition = self.name()?;
            let _ = self.directives()?;
            let sels = self.selection_set()?;
            return Ok(Def::Fragment(name, Fragment { condition, sels }));
        }
        self.fail("a definition")
    }
    fn type_ref(&mut self) -> Result<String, Syntax> {
        let mut s = if self.is("[") {
            self.next();
            let a = self.type_ref()?;
            self.expect("]")?;
            format!("[{a}]")
        } else {
            self.name()?
        };
        if self.is("!") {
            self.next();
            s.push('!');
        }
        Ok(s)
    }
    fn directives(&mut self) -> Result<Vec<Dir>, Syntax> {
        let mut v = vec![];
        while self.is("@") {
            self.next();
            let name = self.name()?;
            let args = if self.is("(") {
                self.arguments()?
            } else {
                vec![]
            };
            v.push(Dir { name, args });
        }
        Ok(v)
    }
    fn arguments(&mut self) -> Result<Vec<Arg>, Syntax> {
        self.expect("(")?;
        let mut v = vec![];
        while !self.is(")") {
            let name = self.name()?;
            self.expect(":")?;
            v.push(Arg {
                name,
                value: self.value(false)?,
            });
            if self.peek().kind == "eof" {
                return self.fail("\")\"");
            }
        }
        self.expect(")")?;
        Ok(v)
    }
    fn value(&mut self, is_const: bool) -> Result<Val, Syntax> {
        let t = self.next();
        match t.kind {
            "int" | "float" | "string" => Ok(Val::Atom(t.kind.into(), t.text)),
            "name" => {
                if t.text == "null" {
                    Ok(Val::Null)
                } else if t.text == "true" || t.text == "false" {
                    Ok(Val::Atom("boolean".into(), t.text))
                } else {
                    Ok(Val::Atom("enum".into(), t.text))
                }
            }
            "punct" if t.text == "$" && !is_const => Ok(Val::Var(self.name()?)),
            "punct" if t.text == "[" => {
                let mut a = vec![];
                while !self.is("]") {
                    a.push(self.value(is_const)?);
                }
                self.expect("]")?;
                Ok(Val::List(a))
            }
            "punct" if t.text == "{" => {
                let mut a = vec![];
                while !self.is("}") {
                    let k = self.name()?;
                    self.expect(":")?;
                    a.push((k, self.value(is_const)?));
                }
                self.expect("}")?;
                Ok(Val::Obj(a))
            }
            _ => {
                self.p = self.p.saturating_sub(1);
                self.fail("a value")
            }
        }
    }
    fn selection_set(&mut self) -> Result<Vec<Sel>, Syntax> {
        self.expect("{")?;
        let mut out = vec![];
        while !self.is("}") {
            if self.peek().kind == "eof" {
                return self.fail("\"}\"");
            }
            out.push(self.selection()?);
        }
        self.expect("}")?;
        if out.is_empty() {
            return self.fail("a selection");
        }
        Ok(out)
    }
    fn selection(&mut self) -> Result<Sel, Syntax> {
        if self.is("...") {
            let start = self.next();
            if self.peek().kind == "name" && self.peek().text != "on" {
                let name = self.name()?;
                return Ok(Sel::Spread(name, self.directives()?, start.line, start.col));
            }
            let condition = if self.peek().kind == "name" && self.peek().text == "on" {
                self.next();
                Some(self.name()?)
            } else {
                None
            };
            let d = self.directives()?;
            return Ok(Sel::Inline(condition, d, self.selection_set()?));
        }
        let first = self.peek().clone();
        let mut name = self.name()?;
        let mut alias = None;
        if self.is(":") {
            self.next();
            alias = Some(name);
            name = self.name()?;
        }
        let args = if self.is("(") {
            self.arguments()?
        } else {
            vec![]
        };
        let dirs = self.directives()?;
        let sels = if self.is("{") {
            Some(self.selection_set()?)
        } else {
            None
        };
        Ok(Sel::Field(Field {
            name,
            alias,
            args,
            dirs,
            sels,
            line: first.line,
            col: first.col,
        }))
    }
}

fn jstr(v: &Value) -> String {
    v.as_str()
        .map(str::to_string)
        .unwrap_or_else(|| v.to_string())
}
fn enc64(bytes: &[u8]) -> String {
    const T: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut o = String::new();
    let mut i = 0;
    while i < bytes.len() {
        let a = bytes[i] as u32;
        let b = if i + 1 < bytes.len() {
            bytes[i + 1] as u32
        } else {
            0
        };
        let c = if i + 2 < bytes.len() {
            bytes[i + 2] as u32
        } else {
            0
        };
        let n = (a << 16) | (b << 8) | c;
        o.push(T[((n >> 18) & 63) as usize] as char);
        o.push(T[((n >> 12) & 63) as usize] as char);
        if i + 1 < bytes.len() {
            o.push(T[((n >> 6) & 63) as usize] as char)
        } else {
            o.push('=')
        }
        if i + 2 < bytes.len() {
            o.push(T[(n & 63) as usize] as char)
        } else {
            o.push('=')
        }
        i += 3;
    }
    o
}
fn dec64(s: &str) -> Option<Vec<u8>> {
    let mut out = vec![];
    let mut acc = 0u32;
    let mut bits = 0;
    for b in s.bytes() {
        if b == b'=' {
            break;
        }
        let n = match b {
            b'A'..=b'Z' => b - b'A',
            b'a'..=b'z' => b - b'a' + 26,
            b'0'..=b'9' => b - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            b'-' => 62,
            b'_' => 63,
            _ => return None,
        } as u32;
        acc = (acc << 6) | n;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push(((acc >> bits) & 255) as u8);
        }
    }
    Some(out)
}
fn gid(prefix: &str, key: &str) -> String {
    let text = format!("gitforge:{prefix}:{key}");
    format!(
        "{prefix}_{}",
        enc64(text.as_bytes())
            .trim_end_matches('=')
            .replace('+', "-")
            .replace('/', "_")
    )
}
fn parse_gid(id: &str) -> Option<(String, String)> {
    let (prefix, body) = id.split_once('_')?;
    let raw = dec64(body)?;
    let text = String::from_utf8(raw).ok()?;
    let expected = format!("gitforge:{prefix}:");
    Some((
        prefix.to_string(),
        text.strip_prefix(&expected)?.to_string(),
    ))
}
fn user() -> Value {
    json!({"__typename":"User"})
}
fn repo_val(repo: &Repo) -> Value {
    json!({"__typename":"Repository","_repo":repo.id})
}
fn issue_val(repo: &Repo, issue: &Issue) -> Value {
    json!({"__typename":"Issue","_repo":repo.id,"_number":issue.number})
}
fn pull_val(repo: &Repo, pull: &Pull) -> Value {
    json!({"__typename":"PullRequest","_repo":repo.id,"_number":pull.number})
}
fn cv(typename: &str, extra: Value) -> Value {
    let mut m = extra.as_object().cloned().unwrap_or_default();
    m.insert("__typename".into(), json!(typename));
    Value::Object(m)
}
fn type_kind(name: &str) -> Option<&'static str> {
    Some(match name {
        "Node" | "GitObject" => "INTERFACE",
        "IssueState"
        | "IssueStateReason"
        | "PullRequestState"
        | "MergeableState"
        | "PullRequestMergeMethod"
        | "RepositoryPermission"
        | "RepositoryVisibility"
        | "SearchType"
        | "IssueOrderField"
        | "LabelOrderField"
        | "OrderDirection"
        | "PatchStatus"
        | "__TypeKind" => "ENUM",
        "IssueOrder"
        | "LabelOrder"
        | "IssueFilters"
        | "CreateIssueInput"
        | "CloseIssueInput"
        | "ReopenIssueInput"
        | "UpdateIssueInput"
        | "AddCommentInput"
        | "CreatePullRequestInput"
        | "MergePullRequestInput"
        | "ClosePullRequestInput"
        | "CreateRepositoryInput" => "INPUT_OBJECT",
        "String" | "Int" | "Float" | "Boolean" | "ID" | "DateTime" | "URI" | "GitObjectID" => {
            "SCALAR"
        }
        _ => "OBJECT",
    })
}

// Ordered fields are also the source for introspection. Order matches the reference TypeScript schema.
fn fields(ty: &str) -> &'static [(&'static str, &'static str)] {
    match ty {
        "Query" => &[
            ("viewer", "User!"),
            ("user", "User"),
            ("repositoryOwner", "User"),
            ("repository", "Repository"),
            ("node", "Node"),
            ("nodes", "[Node]!"),
            ("rateLimit", "RateLimit"),
            ("__type", "__Type"),
            ("__schema", "__Schema!"),
        ],
        "Mutation" => &[
            ("createIssue", "CreateIssuePayload"),
            ("closeIssue", "CloseIssuePayload"),
            ("reopenIssue", "ReopenIssuePayload"),
            ("updateIssue", "UpdateIssuePayload"),
            ("addComment", "AddCommentPayload"),
            ("createPullRequest", "CreatePullRequestPayload"),
            ("mergePullRequest", "MergePullRequestPayload"),
            ("closePullRequest", "ClosePullRequestPayload"),
            ("createRepository", "CreateRepositoryPayload"),
        ],
        "User" => &[
            ("id", "ID!"),
            ("databaseId", "Int"),
            ("login", "String!"),
            ("name", "String"),
            ("email", "String!"),
            ("url", "URI!"),
            ("avatarUrl", "URI!"),
        ],
        "Repository" => &[
            ("id", "ID!"),
            ("databaseId", "Int"),
            ("name", "String!"),
            ("nameWithOwner", "String!"),
            ("owner", "User!"),
            ("description", "String"),
            ("url", "URI!"),
            ("sshUrl", "String!"),
            ("isPrivate", "Boolean!"),
            ("visibility", "RepositoryVisibility!"),
            ("isFork", "Boolean!"),
            ("isArchived", "Boolean!"),
            ("isEmpty", "Boolean!"),
            ("createdAt", "DateTime!"),
            ("updatedAt", "DateTime!"),
            ("pushedAt", "DateTime"),
            ("stargazerCount", "Int!"),
            ("forkCount", "Int!"),
            ("viewerPermission", "RepositoryPermission"),
            ("viewerCanAdminister", "Boolean!"),
            ("hasIssuesEnabled", "Boolean!"),
            ("hasWikiEnabled", "Boolean!"),
            ("mergeCommitAllowed", "Boolean!"),
            ("squashMergeAllowed", "Boolean!"),
            ("rebaseMergeAllowed", "Boolean!"),
            ("defaultBranchRef", "Ref"),
            ("ref", "Ref"),
            ("refs", "RefConnection"),
            ("issue", "Issue"),
            ("issueOrPullRequest", "Node"),
            ("issues", "IssueConnection!"),
            ("pullRequest", "PullRequest"),
            ("pullRequests", "PullRequestConnection!"),
            ("labels", "LabelConnection"),
            ("label", "Label"),
        ],
        "Issue" => &[
            ("id", "ID!"),
            ("databaseId", "Int"),
            ("number", "Int!"),
            ("title", "String!"),
            ("body", "String!"),
            ("bodyText", "String!"),
            ("state", "IssueState!"),
            ("stateReason", "IssueStateReason"),
            ("closed", "Boolean!"),
            ("closedAt", "DateTime"),
            ("createdAt", "DateTime!"),
            ("updatedAt", "DateTime!"),
            ("locked", "Boolean!"),
            ("url", "URI!"),
            ("author", "User"),
            ("repository", "Repository!"),
            ("labels", "LabelConnection"),
            ("comments", "IssueCommentConnection!"),
            ("assignees", "UserConnection!"),
        ],
        "PullRequest" => &[
            ("id", "ID!"),
            ("databaseId", "Int"),
            ("number", "Int!"),
            ("title", "String!"),
            ("body", "String!"),
            ("state", "PullRequestState!"),
            ("merged", "Boolean!"),
            ("mergedAt", "DateTime"),
            ("closed", "Boolean!"),
            ("closedAt", "DateTime"),
            ("createdAt", "DateTime!"),
            ("updatedAt", "DateTime!"),
            ("isDraft", "Boolean!"),
            ("url", "URI!"),
            ("author", "User"),
            ("repository", "Repository!"),
            ("headRefName", "String!"),
            ("baseRefName", "String!"),
            ("headRefOid", "GitObjectID!"),
            ("baseRefOid", "GitObjectID!"),
            ("headRef", "Ref"),
            ("baseRef", "Ref"),
            ("mergeable", "MergeableState!"),
            ("mergeCommit", "Commit"),
            ("additions", "Int!"),
            ("deletions", "Int!"),
            ("changedFiles", "Int!"),
            ("files", "PullRequestChangedFileConnection"),
            ("commits", "PullRequestCommitConnection!"),
            ("comments", "IssueCommentConnection!"),
            ("labels", "LabelConnection"),
        ],
        "IssueComment" => &[
            ("id", "ID!"),
            ("databaseId", "Int"),
            ("body", "String!"),
            ("createdAt", "DateTime!"),
            ("updatedAt", "DateTime!"),
            ("url", "URI!"),
            ("author", "User"),
        ],
        "Label" => &[
            ("id", "ID!"),
            ("name", "String!"),
            ("color", "String!"),
            ("description", "String"),
        ],
        "Ref" => &[
            ("id", "ID!"),
            ("name", "String!"),
            ("prefix", "String!"),
            ("target", "GitObject"),
            ("repository", "Repository!"),
        ],
        "Commit" => &[
            ("id", "ID!"),
            ("oid", "GitObjectID!"),
            ("abbreviatedOid", "String!"),
            ("message", "String!"),
            ("messageHeadline", "String!"),
            ("committedDate", "DateTime!"),
            ("authoredDate", "DateTime!"),
            ("url", "URI!"),
        ],
        "PageInfo" => &[
            ("hasNextPage", "Boolean!"),
            ("hasPreviousPage", "Boolean!"),
            ("startCursor", "String"),
            ("endCursor", "String"),
        ],
        "RateLimit" => &[
            ("limit", "Int!"),
            ("cost", "Int!"),
            ("remaining", "Int!"),
            ("used", "Int!"),
            ("resetAt", "DateTime!"),
            ("nodeCount", "Int!"),
        ],
        "__Type" => &[
            ("kind", "__TypeKind!"),
            ("name", "String"),
            ("description", "String"),
            ("fields", "[__Field!]"),
            ("interfaces", "[__Type!]"),
            ("possibleTypes", "[__Type!]"),
            ("enumValues", "[__EnumValue!]"),
            ("inputFields", "[__InputValue!]"),
            ("ofType", "__Type"),
            ("specifiedByURL", "String"),
        ],
        "__Schema" => &[
            ("types", "[__Type!]!"),
            ("queryType", "__Type!"),
            ("mutationType", "__Type"),
            ("subscriptionType", "__Type"),
            ("directives", "[__Directive!]!"),
        ],
        "__Field" => &[
            ("name", "String!"),
            ("description", "String"),
            ("args", "[__InputValue!]!"),
            ("type", "__Type!"),
            ("isDeprecated", "Boolean!"),
            ("deprecationReason", "String"),
        ],
        "__InputValue" => &[
            ("name", "String!"),
            ("description", "String"),
            ("type", "__Type!"),
            ("defaultValue", "String"),
        ],
        "__EnumValue" => &[
            ("name", "String!"),
            ("description", "String"),
            ("isDeprecated", "Boolean!"),
            ("deprecationReason", "String"),
        ],
        "PullRequestChangedFile" => &[
            ("path", "String!"),
            ("additions", "Int!"),
            ("deletions", "Int!"),
            ("changeType", "PatchStatus!"),
        ],
        "PullRequestCommit" => &[("commit", "Commit!")],
        "IssueConnection" => &[
            ("nodes", "[Issue]"),
            ("edges", "[IssueEdge]"),
            ("pageInfo", "PageInfo!"),
            ("totalCount", "Int!"),
        ],
        "PullRequestConnection" => &[
            ("nodes", "[PullRequest]"),
            ("edges", "[PullRequestEdge]"),
            ("pageInfo", "PageInfo!"),
            ("totalCount", "Int!"),
        ],
        "IssueCommentConnection" => &[
            ("nodes", "[IssueComment]"),
            ("edges", "[IssueCommentEdge]"),
            ("pageInfo", "PageInfo!"),
            ("totalCount", "Int!"),
        ],
        "LabelConnection" => &[
            ("nodes", "[Label]"),
            ("edges", "[LabelEdge]"),
            ("pageInfo", "PageInfo!"),
            ("totalCount", "Int!"),
        ],
        "RefConnection" => &[
            ("nodes", "[Ref]"),
            ("edges", "[RefEdge]"),
            ("pageInfo", "PageInfo!"),
            ("totalCount", "Int!"),
        ],
        "UserConnection" => &[
            ("nodes", "[User]"),
            ("edges", "[UserEdge]"),
            ("pageInfo", "PageInfo!"),
            ("totalCount", "Int!"),
        ],
        "PullRequestChangedFileConnection" => &[
            ("nodes", "[PullRequestChangedFile]"),
            ("edges", "[PullRequestChangedFileEdge]"),
            ("pageInfo", "PageInfo!"),
            ("totalCount", "Int!"),
        ],
        "PullRequestCommitConnection" => &[
            ("nodes", "[PullRequestCommit]"),
            ("edges", "[PullRequestCommitEdge]"),
            ("pageInfo", "PageInfo!"),
            ("totalCount", "Int!"),
        ],
        "IssueEdge" => &[("node", "Issue"), ("cursor", "String!")],
        "PullRequestEdge" => &[("node", "PullRequest"), ("cursor", "String!")],
        "IssueCommentEdge" => &[("node", "IssueComment"), ("cursor", "String!")],
        "LabelEdge" => &[("node", "Label"), ("cursor", "String!")],
        "RefEdge" => &[("node", "Ref"), ("cursor", "String!")],
        "UserEdge" => &[("node", "User"), ("cursor", "String!")],
        "PullRequestChangedFileEdge" => {
            &[("node", "PullRequestChangedFile"), ("cursor", "String!")]
        }
        "PullRequestCommitEdge" => &[("node", "PullRequestCommit"), ("cursor", "String!")],
        "CreateIssuePayload" => &[("issue", "Issue"), ("clientMutationId", "String")],
        "CloseIssuePayload" | "ReopenIssuePayload" | "UpdateIssuePayload" => {
            &[("issue", "Issue"), ("clientMutationId", "String")]
        }
        "AddCommentPayload" => &[
            ("commentEdge", "IssueCommentEdge"),
            ("subject", "Node"),
            ("clientMutationId", "String"),
        ],
        "CreatePullRequestPayload" | "MergePullRequestPayload" | "ClosePullRequestPayload" => &[
            ("pullRequest", "PullRequest"),
            ("clientMutationId", "String"),
        ],
        "CreateRepositoryPayload" => {
            &[("repository", "Repository"), ("clientMutationId", "String")]
        }
        "Node" => &[("id", "ID!")],
        "GitObject" => &[
            ("id", "ID!"),
            ("oid", "GitObjectID!"),
            ("abbreviatedOid", "String!"),
        ],
        "__Directive" => &[
            ("name", "String!"),
            ("description", "String"),
            ("locations", "[String!]!"),
            ("args", "[__InputValue!]!"),
        ],
        _ => &[],
    }
}

fn enum_vals(ty: &str) -> &'static [&'static str] {
    match ty {
        "IssueState" => &["OPEN", "CLOSED"],
        "IssueStateReason" => &["COMPLETED", "NOT_PLANNED", "REOPENED", "DUPLICATE"],
        "PullRequestState" => &["OPEN", "CLOSED", "MERGED"],
        "MergeableState" => &["MERGEABLE", "CONFLICTING", "UNKNOWN"],
        "PullRequestMergeMethod" => &["MERGE", "SQUASH", "REBASE"],
        "RepositoryPermission" => &["ADMIN", "MAINTAIN", "WRITE", "TRIAGE", "READ"],
        "RepositoryVisibility" => &["PRIVATE", "PUBLIC", "INTERNAL"],
        "SearchType" => &[
            "ISSUE",
            "ISSUE_ADVANCED",
            "REPOSITORY",
            "USER",
            "DISCUSSION",
        ],
        "IssueOrderField" => &["CREATED_AT", "UPDATED_AT", "COMMENTS"],
        "LabelOrderField" => &["NAME", "CREATED_AT"],
        "OrderDirection" => &["ASC", "DESC"],
        "PatchStatus" => &[
            "ADDED", "DELETED", "RENAMED", "COPIED", "MODIFIED", "CHANGED",
        ],
        "__TypeKind" => &[
            "SCALAR",
            "OBJECT",
            "INTERFACE",
            "UNION",
            "ENUM",
            "INPUT_OBJECT",
            "LIST",
            "NON_NULL",
        ],
        _ => &[],
    }
}
fn all_type_names() -> Vec<&'static str> {
    vec![
        "Query",
        "Mutation",
        "Node",
        "GitObject",
        "User",
        "Repository",
        "Ref",
        "Commit",
        "Issue",
        "IssueComment",
        "Label",
        "PullRequest",
        "PullRequestChangedFile",
        "PullRequestCommit",
        "PageInfo",
        "RateLimit",
        "CreateIssuePayload",
        "CloseIssuePayload",
        "ReopenIssuePayload",
        "UpdateIssuePayload",
        "AddCommentPayload",
        "CreatePullRequestPayload",
        "MergePullRequestPayload",
        "ClosePullRequestPayload",
        "CreateRepositoryPayload",
        "IssueState",
        "IssueStateReason",
        "PullRequestState",
        "MergeableState",
        "PullRequestMergeMethod",
        "RepositoryPermission",
        "RepositoryVisibility",
        "SearchType",
        "IssueOrderField",
        "LabelOrderField",
        "OrderDirection",
        "PatchStatus",
        "__TypeKind",
        "IssueOrder",
        "LabelOrder",
        "IssueFilters",
        "CreateIssueInput",
        "CloseIssueInput",
        "ReopenIssueInput",
        "UpdateIssueInput",
        "AddCommentInput",
        "CreatePullRequestInput",
        "MergePullRequestInput",
        "ClosePullRequestInput",
        "CreateRepositoryInput",
        "String",
        "Int",
        "Float",
        "Boolean",
        "ID",
        "DateTime",
        "URI",
        "GitObjectID",
        "__Schema",
        "__Directive",
        "__Field",
        "__InputValue",
        "__EnumValue",
        "IssueConnection",
        "IssueEdge",
        "PullRequestConnection",
        "PullRequestEdge",
        "IssueCommentConnection",
        "IssueCommentEdge",
        "LabelConnection",
        "LabelEdge",
        "PullRequestChangedFileConnection",
        "PullRequestChangedFileEdge",
        "RefConnection",
        "RefEdge",
        "UserConnection",
        "UserEdge",
        "PullRequestCommitConnection",
        "PullRequestCommitEdge",
    ]
}

fn arg_defs(ty: &str, field: &str) -> &'static [(&'static str, &'static str)] {
    match (ty, field) {
        ("Query", "user" | "repositoryOwner") => &[("login", "String!")],
        ("Query", "repository") => &[
            ("owner", "String!"),
            ("name", "String!"),
            ("followRenames", "Boolean"),
        ],
        ("Query", "node") => &[("id", "ID!")],
        ("Query", "nodes") => &[("ids", "[ID!]!")],
        ("Query", "__type") => &[("name", "String!")],
        ("Repository", "issue" | "issueOrPullRequest" | "pullRequest") => &[("number", "Int!")],
        ("Repository", "issues") => &[
            ("first", "Int"),
            ("last", "Int"),
            ("after", "String"),
            ("before", "String"),
            ("states", "[IssueState!]"),
            ("labels", "[String!]"),
            ("orderBy", "IssueOrder"),
            ("filterBy", "IssueFilters"),
        ],
        ("Repository", "pullRequests") => &[
            ("first", "Int"),
            ("last", "Int"),
            ("after", "String"),
            ("before", "String"),
            ("states", "[PullRequestState!]"),
            ("headRefName", "String"),
            ("baseRefName", "String"),
            ("labels", "[String!]"),
            ("orderBy", "IssueOrder"),
        ],
        ("Issue", "labels" | "comments") | ("PullRequest", "files" | "commits" | "comments") => &[
            ("first", "Int"),
            ("last", "Int"),
            ("after", "String"),
            ("before", "String"),
        ],
        ("Repository", "ref") => &[("qualifiedName", "String!")],
        ("Repository", "refs") => &[
            ("first", "Int"),
            ("last", "Int"),
            ("after", "String"),
            ("before", "String"),
            ("refPrefix", "String!"),
            ("query", "String"),
        ],
        ("Repository", "label") => &[("name", "String!")],
        ("Repository", "labels") => &[
            ("first", "Int"),
            ("last", "Int"),
            ("after", "String"),
            ("before", "String"),
            ("query", "String"),
            ("orderBy", "LabelOrder"),
        ],
        ("User", "avatarUrl") => &[("size", "Int")],
        ("__Type", "fields" | "enumValues") => &[("includeDeprecated", "Boolean")],
        ("Mutation", _) => &[("input", "Input!")],
        _ => &[],
    }
}
fn type_exists(ty: &str) -> bool {
    all_type_names().contains(&ty)
}
fn named(ty: &str) -> String {
    ty.chars()
        .filter(|c| *c != '[' && *c != ']' && *c != '!')
        .collect()
}
fn type_ref(ty: &str) -> Value {
    if let Some(a) = ty.strip_suffix('!') {
        json!({"__typename":"__Type","kind":"NON_NULL","ofType":type_ref(a)})
    } else if ty.starts_with('[') {
        json!({"__typename":"__Type","kind":"LIST","ofType":type_ref(&ty[1..ty.len()-1])})
    } else {
        json!({"__typename":"__Type","kind":type_kind(ty).unwrap_or("OBJECT"),"name":ty,"_type_name":ty})
    }
}
fn args_for_field(ty: &str, name: &str) -> Vec<(&'static str, &'static str)> {
    arg_defs(ty, name).to_vec()
}

fn input_fields(name: &str) -> &'static [(&'static str, &'static str)] {
    match name {
        "IssueOrder" => &[
            ("field", "IssueOrderField!"),
            ("direction", "OrderDirection!"),
        ],
        "LabelOrder" => &[
            ("field", "LabelOrderField!"),
            ("direction", "OrderDirection!"),
        ],
        "IssueFilters" => &[
            ("states", "[IssueState!]"),
            ("labels", "[String!]"),
            ("assignee", "String"),
            ("createdBy", "String"),
        ],
        "CreateIssueInput" => &[
            ("repositoryId", "ID!"),
            ("title", "String!"),
            ("body", "String"),
            ("labelIds", "[ID!]"),
            ("assigneeIds", "[ID!]"),
            ("clientMutationId", "String"),
        ],
        "CloseIssueInput" => &[
            ("issueId", "ID!"),
            ("stateReason", "IssueStateReason"),
            ("clientMutationId", "String"),
        ],
        "ReopenIssueInput" => &[("issueId", "ID!"), ("clientMutationId", "String")],
        "UpdateIssueInput" => &[
            ("id", "ID!"),
            ("title", "String"),
            ("body", "String"),
            ("state", "IssueState"),
            ("clientMutationId", "String"),
        ],
        "AddCommentInput" => &[
            ("subjectId", "ID!"),
            ("body", "String!"),
            ("clientMutationId", "String"),
        ],
        "CreatePullRequestInput" => &[
            ("repositoryId", "ID!"),
            ("baseRefName", "String!"),
            ("headRefName", "String!"),
            ("headRepositoryId", "ID"),
            ("title", "String!"),
            ("body", "String"),
            ("draft", "Boolean"),
            ("maintainerCanModify", "Boolean"),
            ("clientMutationId", "String"),
        ],
        "MergePullRequestInput" => &[
            ("pullRequestId", "ID!"),
            ("mergeMethod", "PullRequestMergeMethod"),
            ("commitHeadline", "String"),
            ("commitBody", "String"),
            ("expectedHeadOid", "GitObjectID"),
            ("authorEmail", "String"),
            ("clientMutationId", "String"),
        ],
        "ClosePullRequestInput" => &[("pullRequestId", "ID!"), ("clientMutationId", "String")],
        "CreateRepositoryInput" => &[
            ("name", "String!"),
            ("description", "String"),
            ("visibility", "RepositoryVisibility!"),
            ("clientMutationId", "String"),
        ],
        _ => &[],
    }
}

fn composite(name: &str) -> bool {
    matches!(type_kind(name), Some("OBJECT" | "INTERFACE"))
}
fn type_value(name: &str) -> Value {
    json!({"__typename":"__Type","_type_name":name,"kind":type_kind(name).unwrap_or("OBJECT"),"name":name,"description":null})
}

fn connection(name: &str, items: Vec<Value>, args: &Map<String, Value>) -> Result<Value, String> {
    let dec_cursor = |key: &str| -> Result<Option<usize>, String> {
        let Some(s) = args.get(key).and_then(Value::as_str) else {
            return Ok(None);
        };
        if s.is_empty() {
            return Ok(None);
        }
        let text = String::from_utf8(dec64(s).unwrap_or_default()).unwrap_or_default();
        let n = text
            .strip_prefix("cursor:v2:")
            .and_then(|x| x.parse::<usize>().ok())
            .ok_or_else(|| format!("`{s}` does not appear to be a valid cursor."))?;
        Ok(Some(n))
    };
    let mut start = 0usize;
    let mut end = items.len();
    if let Some(n) = dec_cursor("after")? {
        start = start.max(n.saturating_add(1));
    }
    if let Some(n) = dec_cursor("before")? {
        end = end.min(n);
    }
    if let Some(n) = args.get("first").and_then(Value::as_u64) {
        end = end.min(start.saturating_add(n as usize));
    } else if args.get("first").is_some() {
        return Err("`first` must be a non-negative integer".into());
    }
    if let Some(n) = args.get("last").and_then(Value::as_u64) {
        start = start.max(end.saturating_sub(n as usize));
    } else if args.get("last").is_some() {
        return Err("`last` must be a non-negative integer".into());
    }
    let end = end.max(start).min(items.len());
    let start = start.min(end);
    let slice = items[start..end].to_vec();
    let cur = |i: usize| enc64(format!("cursor:v2:{i}").as_bytes());
    let base = name.strip_suffix("Connection").unwrap_or(name);
    let edges:Vec<Value>=slice.iter().enumerate().map(|(i,node)|json!({"__typename":format!("{base}Edge"),"node":node,"cursor":cur(start+i)})).collect();
    Ok(
        json!({"__typename":name,"nodes":slice,"edges":edges,"totalCount":items.len(),"pageInfo":{"__typename":"PageInfo","hasNextPage":end<items.len(),"hasPreviousPage":start>0,"startCursor":if end>start{Value::String(cur(start))}else{Value::Null},"endCursor":if end>start{Value::String(cur(end-1))}else{Value::Null}}}),
    )
}

fn issue_state(i: &Issue) -> &'static str {
    if i.state == "open" {
        "OPEN"
    } else {
        "CLOSED"
    }
}
fn pull_state(p: &Pull) -> &'static str {
    if p.merged {
        "MERGED"
    } else if p.state == "open" {
        "OPEN"
    } else {
        "CLOSED"
    }
}
fn sort_issues(items: &mut [Issue], order: &Value) {
    let field = order
        .get("field")
        .and_then(Value::as_str)
        .unwrap_or("CREATED_AT");
    let desc = order.get("direction").and_then(Value::as_str) == Some("DESC");
    items.sort_by(|a, b| {
        let c = match field {
            "UPDATED_AT" => a.updated_at.cmp(&b.updated_at),
            "COMMENTS" => a.comments.len().cmp(&b.comments.len()),
            _ => a.created_at.cmp(&b.created_at),
        }
        .then(a.number.cmp(&b.number));
        if desc {
            c.reverse()
        } else {
            c
        }
    });
}
fn error_type(status: u16) -> &'static str {
    match status {
        404 => "NOT_FOUND",
        422 | 409 | 405 => "UNPROCESSABLE",
        403 => "FORBIDDEN",
        _ => "INTERNAL",
    }
}
fn api_err(e: ApiError) -> (u16, String) {
    if e.errors.is_empty() {
        return (e.status, e.message);
    }
    let details = e
        .errors
        .iter()
        .map(|entry| {
            if let Some(message) = &entry.message {
                message.clone()
            } else {
                json!({
                    "resource": entry.resource,
                    "code": entry.code,
                    "field": entry.field,
                    "message": entry.message,
                    "value": entry.value,
                })
                .to_string()
            }
        })
        .collect::<Vec<_>>()
        .join("; ");
    (e.status, format!("{}: {details}", e.message))
}

fn node_from_id(f: &Forge, id: &str) -> Option<Value> {
    let (prefix, key) = parse_gid(id)?;
    let n = key.parse::<u64>().ok()?;
    if prefix == "U" {
        return (n == f.user.id).then(user);
    }
    if prefix == "R" {
        return f.repo_by_id(n).as_ref().map(repo_val);
    }
    for repo in f.list_repos() {
        match prefix.as_str() {
            "I" => {
                for issue in repo.issues.values() {
                    if issue.id == n {
                        return Some(issue_val(&repo, issue));
                    }
                }
            }
            "PR" => {
                for pull in repo.pulls.values() {
                    if pull.id == n {
                        return Some(pull_val(&repo, pull));
                    }
                }
            }
            "LA" => {
                for label in repo.labels.values() {
                    if label.id == n {
                        return Some(cv("Label", json!({"_repo":repo.id,"_label":label.id})));
                    }
                }
            }
            "IC" => {
                for issue in repo.issues.values() {
                    for c in &issue.comments {
                        if c.id == n {
                            return Some(cv(
                                "IssueComment",
                                json!({"_repo":repo.id,"_number":issue.number,"_comment":c.id}),
                            ));
                        }
                    }
                }
                for pull in repo.pulls.values() {
                    for c in &pull.comments {
                        if c.id == n {
                            return Some(cv(
                                "IssueComment",
                                json!({"_repo":repo.id,"_number":pull.number,"_comment":c.id}),
                            ));
                        }
                    }
                }
            }
            _ => {}
        }
    }
    None
}

fn runtime_type(parent: &Value, fallback: &str) -> String {
    parent
        .get("__typename")
        .and_then(Value::as_str)
        .unwrap_or(fallback)
        .to_string()
}
fn field_type(ty: &str, name: &str) -> Option<&'static str> {
    if name == "__typename" {
        return Some("String!");
    }
    let node = match ty {
        "IssueConnection" => Some("Issue"),
        "PullRequestConnection" => Some("PullRequest"),
        "IssueCommentConnection" => Some("IssueComment"),
        "LabelConnection" => Some("Label"),
        "RefConnection" => Some("Ref"),
        "UserConnection" => Some("User"),
        "PullRequestChangedFileConnection" => Some("PullRequestChangedFile"),
        "PullRequestCommitConnection" => Some("PullRequestCommit"),
        _ => None,
    };
    if let Some(node) = node {
        return match name {
            "nodes" => match node {
                "Issue" => Some("[Issue]"),
                "PullRequest" => Some("[PullRequest]"),
                "IssueComment" => Some("[IssueComment]"),
                "Label" => Some("[Label]"),
                "Ref" => Some("[Ref]"),
                "User" => Some("[User]"),
                "PullRequestChangedFile" => Some("[PullRequestChangedFile]"),
                _ => Some("[PullRequestCommit]"),
            },
            "edges" => match node {
                "Issue" => Some("[IssueEdge]"),
                "PullRequest" => Some("[PullRequestEdge]"),
                "IssueComment" => Some("[IssueCommentEdge]"),
                "Label" => Some("[LabelEdge]"),
                "Ref" => Some("[RefEdge]"),
                "User" => Some("[UserEdge]"),
                "PullRequestChangedFile" => Some("[PullRequestChangedFileEdge]"),
                _ => Some("[PullRequestCommitEdge]"),
            },
            "pageInfo" => Some("PageInfo!"),
            "totalCount" => Some("Int!"),
            _ => None,
        };
    }
    let edge_node = match ty {
        "IssueEdge" => Some("Issue"),
        "PullRequestEdge" => Some("PullRequest"),
        "IssueCommentEdge" => Some("IssueComment"),
        "LabelEdge" => Some("Label"),
        "RefEdge" => Some("Ref"),
        "UserEdge" => Some("User"),
        "PullRequestChangedFileEdge" => Some("PullRequestChangedFile"),
        "PullRequestCommitEdge" => Some("PullRequestCommit"),
        _ => None,
    };
    if let Some(node) = edge_node {
        return match name {
            "node" => match node {
                "Issue" => Some("Issue"),
                "PullRequest" => Some("PullRequest"),
                "IssueComment" => Some("IssueComment"),
                "Label" => Some("Label"),
                "Ref" => Some("Ref"),
                "User" => Some("User"),
                "PullRequestChangedFile" => Some("PullRequestChangedFile"),
                _ => Some("PullRequestCommit"),
            },
            "cursor" => Some("String!"),
            _ => None,
        };
    }
    fields(ty).iter().find(|(n, _)| *n == name).map(|(_, t)| *t)
}
fn validate(
    sels: &[Sel],
    ty: &str,
    frags: &HashMap<String, Fragment>,
    seen: &mut HashSet<String>,
    errs: &mut Vec<Value>,
) {
    for s in sels {
        match s {
            Sel::Field(n) => {
                if n.name == "__typename" {
                    if n.sels.is_some() {
                        errs.push(json!({"message":"Selections can't be made on scalars (field '__typename' returns String but has selections)","locations":[{"line":n.line,"column":n.col}]}));
                    }
                    continue;
                }
                let Some(ft) = field_type(ty, &n.name) else {
                    errs.push(json!({"message":format!("Field '{}' doesn't exist on type '{ty}'",n.name),"locations":[{"line":n.line,"column":n.col}],"path":[n.alias.as_deref().unwrap_or(&n.name)],"extensions":{"code":"undefinedField","typeName":ty,"fieldName":n.name}}));
                    continue;
                };
                let target = named(ft);
                let comp = composite(&target);
                if comp && n.sels.is_none() {
                    errs.push(json!({"message":format!("Field must have selections (field '{}' returns {target} but has no selections. Did you mean '{} {{ ... }}'?)",n.name,n.name),"locations":[{"line":n.line,"column":n.col}],"extensions":{"code":"selectionMismatch"}}));
                } else if !comp && n.sels.is_some() {
                    errs.push(json!({"message":format!("Selections can't be made on scalars (field '{}' returns {target} but has selections)",n.name),"locations":[{"line":n.line,"column":n.col}],"extensions":{"code":"selectionMismatch"}}));
                } else if let Some(children) = &n.sels {
                    validate(children, &target, frags, seen, errs);
                }
                let accepted = arg_defs(ty, &n.name);
                for a in &n.args {
                    if !accepted.iter().any(|(k, _)| *k == a.name) {
                        errs.push(json!({"message":format!("Field '{}' doesn't accept argument '{}'",n.name,a.name),"locations":[{"line":n.line,"column":n.col}],"extensions":{"code":"argumentNotAccepted","name":n.name,"typeName":"Field","argumentName":a.name}}));
                    }
                }
                for (arg, at) in accepted.to_vec() {
                    if at.ends_with('!') && !n.args.iter().any(|a| a.name == arg) {
                        errs.push(json!({"message":format!("Field '{}' is missing required arguments: {arg}",n.name),"locations":[{"line":n.line,"column":n.col}],"extensions":{"code":"missingRequiredArguments","className":"Field","name":n.name,"arguments":arg}}));
                    }
                }
            }
            Sel::Inline(cond, _, children) => {
                let c = cond.as_deref().unwrap_or(ty);
                if !type_exists(c) {
                    errs.push(json!({"message":format!("No such type {c}, so it can't be a fragment condition"),"extensions":{"code":"undefinedType","typeName":c}}));
                } else {
                    validate(children, c, frags, seen, errs);
                }
            }
            Sel::Spread(name, _, l, c) => {
                if let Some(fr) = frags.get(name) {
                    if seen.insert(name.clone()) {
                        validate(&fr.sels, &fr.condition, frags, seen, errs);
                        seen.remove(name);
                    }
                } else {
                    errs.push(json!({"message":format!("Fragment {name} was used, but not defined"),"locations":[{"line":l,"column":c}],"extensions":{"code":"useAndDefineFragment","fragmentName":name}}));
                }
            }
        }
    }
}

struct Exec<'a> {
    forge: &'a mut Forge,
    origin: &'a str,
    variables: Map<String, Value>,
    frags: &'a HashMap<String, Fragment>,
    errors: Vec<Value>,
}
impl Exec<'_> {
    fn eval(&self, v: &Val) -> Value {
        match v {
            Val::Var(n) => self.variables.get(n).cloned().unwrap_or(Value::Null),
            Val::Null => Value::Null,
            Val::Atom(k, s) => match k.as_str() {
                "int" => s.parse::<i64>().map(Value::from).unwrap_or(Value::Null),
                "float" => s
                    .parse::<f64>()
                    .ok()
                    .and_then(serde_json::Number::from_f64)
                    .map(Value::Number)
                    .unwrap_or(Value::Null),
                "boolean" => Value::Bool(s == "true"),
                _ => Value::String(s.clone()),
            },
            Val::List(a) => Value::Array(a.iter().map(|x| self.eval(x)).collect()),
            Val::Obj(a) => {
                let mut m = Map::new();
                for (k, x) in a {
                    m.insert(k.clone(), self.eval(x));
                }
                Value::Object(m)
            }
        }
    }
    fn include(&self, d: &[Dir]) -> bool {
        for dir in d {
            if dir.name == "skip" || dir.name == "include" {
                let b = dir
                    .args
                    .iter()
                    .find(|a| a.name == "if")
                    .map(|a| self.eval(&a.value))
                    .unwrap_or(Value::Bool(false))
                    .as_bool()
                    .unwrap_or(false);
                if dir.name == "skip" && b || dir.name == "include" && !b {
                    return false;
                }
            }
        }
        true
    }
    fn collect(
        &self,
        sels: &[Sel],
        runtime: &str,
        out: &mut Vec<(String, Vec<Field>)>,
        seen: &mut HashSet<String>,
    ) {
        for s in sels {
            match s {
                Sel::Field(f) => {
                    if self.include(&f.dirs) {
                        let key = f.alias.as_deref().unwrap_or(&f.name).to_string();
                        if let Some((_, v)) = out.iter_mut().find(|(k, _)| *k == key) {
                            v.push(f.clone())
                        } else {
                            out.push((key, vec![f.clone()]));
                        }
                    }
                }
                Sel::Inline(cond, d, ch) => {
                    if self.include(d) && cond.as_deref().is_none_or(|c| implements(runtime, c)) {
                        self.collect(ch, runtime, out, seen)
                    }
                }
                Sel::Spread(name, d, _, _) => {
                    if self.include(d) && seen.insert(name.clone()) {
                        if let Some(fr) = self.frags.get(name) {
                            if implements(runtime, &fr.condition) {
                                self.collect(&fr.sels, runtime, out, seen);
                            }
                        }
                        seen.remove(name);
                    }
                }
            }
        }
    }
    fn selection(&mut self, sels: &[Sel], ty: &str, parent: &Value, path: &[Value]) -> Value {
        let rt = runtime_type(parent, ty);
        let mut list = vec![];
        self.collect(sels, &rt, &mut list, &mut HashSet::new());
        let mut out = Map::new();
        for (key, nodes) in list {
            let n = &nodes[0];
            if n.name == "__typename" {
                out.insert(key, Value::String(rt.clone()));
                continue;
            }
            let Some(ft) = field_type(&rt, &n.name) else {
                out.insert(key, Value::Null);
                continue;
            };
            let mut args = Map::new();
            for a in &n.args {
                args.insert(a.name.clone(), self.eval(&a.value));
            }
            let mut fpath = path.to_vec();
            fpath.push(Value::String(key.clone()));
            let result = self.resolve(&rt, &n.name, &args, parent);
            let value = match result {
                Ok(v) => v,
                Err((status, msg)) => {
                    let path_json = Value::Array(fpath.clone());
                    self.errors.push(json!({"type":error_type(status),"path":path_json,"locations":[{"line":n.line,"column":n.col}],"message":msg}));
                    Value::Null
                }
            };
            let mut children = vec![];
            for each in &nodes {
                if let Some(s) = &each.sels {
                    children.extend(s.clone());
                }
            }
            let completed = self.complete(ft, &value, &children, &fpath);
            out.insert(key, completed);
        }
        Value::Object(out)
    }
    fn complete(&mut self, ty: &str, value: &Value, sels: &[Sel], path: &[Value]) -> Value {
        if value.is_null() {
            return Value::Null;
        }
        let inner = ty.strip_suffix('!').unwrap_or(ty);
        if inner.starts_with('[') {
            let Some(arr) = value.as_array() else {
                return Value::Null;
            };
            let mut out = vec![];
            for (i, v) in arr.iter().enumerate() {
                let mut p = path.to_vec();
                p.push(json!(i));
                out.push(self.complete(&inner[1..inner.len() - 1], v, sels, &p));
            }
            return Value::Array(out);
        }
        let t = named(inner);
        if composite(&t) || t.ends_with("Connection") || t.ends_with("Edge") {
            self.selection(sels, &t, value, path)
        } else {
            value.clone()
        }
    }
    fn resolve(
        &mut self,
        ty: &str,
        name: &str,
        a: &Map<String, Value>,
        p: &Value,
    ) -> Result<Value, (u16, String)> {
        if ty == "Query" {
            return match name {
                "viewer" => Ok(user()),
                "user" | "repositoryOwner" => Ok(
                    if self
                        .forge
                        .is_viewer(&a.get("login").map(jstr).unwrap_or_default())
                    {
                        user()
                    } else {
                        Value::Null
                    },
                ),
                "repository" => Ok(self
                    .forge
                    .repo_by_name(
                        &jstr(a.get("owner").unwrap_or(&Value::Null)),
                        &jstr(a.get("name").unwrap_or(&Value::Null)),
                    )
                    .as_ref()
                    .map(repo_val)
                    .unwrap_or(Value::Null)),
                "node" => Ok(
                    node_from_id(self.forge, &jstr(a.get("id").unwrap_or(&Value::Null)))
                        .unwrap_or(Value::Null),
                ),
                "nodes" => Ok(Value::Array(
                    a.get("ids")
                        .and_then(Value::as_array)
                        .unwrap_or(&vec![])
                        .iter()
                        .map(|id| node_from_id(self.forge, &jstr(id)).unwrap_or(Value::Null))
                        .collect(),
                )),
                "rateLimit" => Ok(
                    json!({"__typename":"RateLimit","limit":5000,"cost":1,"remaining":4999,"used":1,"resetAt":"1970-01-01T00:00:00Z","nodeCount":0}),
                ),
                "__type" => {
                    let name = jstr(a.get("name").unwrap_or(&Value::Null));
                    Ok(if type_exists(&name) {
                        type_value(&name)
                    } else {
                        Value::Null
                    })
                }
                "__schema" => Ok(json!({"__typename":"__Schema"})),
                _ => Ok(Value::Null),
            };
        }
        if ty == "Mutation" {
            return self.mutation(name, a);
        }
        if ty == "__Schema" {
            return match name {
                "types" => Ok(Value::Array(
                    all_type_names().iter().map(|n| type_value(n)).collect(),
                )),
                "queryType" => Ok(type_value("Query")),
                "mutationType" => Ok(type_value("Mutation")),
                "subscriptionType" => Ok(Value::Null),
                "directives" => Ok(
                    json!([{"__typename":"__Directive","name":"include","description":null,"locations":["FIELD","FRAGMENT_SPREAD","INLINE_FRAGMENT"],"args":[input_value("if","Boolean!")]},{"__typename":"__Directive","name":"skip","description":null,"locations":["FIELD","FRAGMENT_SPREAD","INLINE_FRAGMENT"],"args":[input_value("if","Boolean!")]}]),
                ),
                _ => Ok(Value::Null),
            };
        }
        if ty == "__Type" {
            let n = p
                .get("_type_name")
                .or_else(|| p.get("name"))
                .and_then(Value::as_str)
                .unwrap_or("");
            return match name {
                "kind" => Ok(p
                    .get("kind")
                    .cloned()
                    .unwrap_or_else(|| json!(type_kind(n).unwrap_or("OBJECT")))),
                "name" => Ok(p.get("name").cloned().unwrap_or_else(|| json!(n))),
                "description" => Ok(p.get("description").cloned().unwrap_or(Value::Null)),
                "fields" => {
                    if type_kind(n) != Some("OBJECT") && type_kind(n) != Some("INTERFACE") {
                        return Ok(Value::Null);
                    }
                    Ok(Value::Array(fields(n).iter().map(|(name,ty)|{let args=args_for_field(n,name).iter().map(|(k,t)|input_value(k,t)).collect::<Vec<_>>();json!({"__typename":"__Field","name":name,"description":null,"args":args,"type":type_ref(ty),"isDeprecated":false,"deprecationReason":null})}).collect()))
                }
                "enumValues" => Ok(if type_kind(n) == Some("ENUM") {
                    Value::Array(enum_vals(n).iter().map(|v|json!({"__typename":"__EnumValue","name":v,"description":null,"isDeprecated":false,"deprecationReason":null})).collect())
                } else {
                    Value::Null
                }),
                "inputFields" => Ok(if type_kind(n) == Some("INPUT_OBJECT") {
                    Value::Array(
                        input_fields(n)
                            .iter()
                            .map(|(k, t)| input_value(k, t))
                            .collect(),
                    )
                } else {
                    Value::Null
                }),
                "interfaces" => Ok(if type_kind(n) == Some("OBJECT") {
                    Value::Array(
                        match n {
                            "User" | "Repository" | "Issue" | "IssueComment" | "Label"
                            | "PullRequest" | "Ref" => vec!["Node"],
                            "Commit" => vec!["Node", "GitObject"],
                            _ => vec![],
                        }
                        .iter()
                        .map(|n| type_value(n))
                        .collect(),
                    )
                } else {
                    Value::Null
                }),
                "possibleTypes" => Ok(if type_kind(n) == Some("INTERFACE") {
                    Value::Array(
                        all_type_names()
                            .into_iter()
                            .filter(|candidate| implements(candidate, n))
                            .map(type_value)
                            .collect(),
                    )
                } else {
                    Value::Null
                }),
                "ofType" => Ok(p.get("ofType").cloned().unwrap_or(Value::Null)),
                "specifiedByURL" => Ok(Value::Null),
                _ => Ok(Value::Null),
            };
        }
        if ty == "__Field" || ty == "__InputValue" || ty == "__EnumValue" || ty == "__Directive" {
            return Ok(p.get(name).cloned().unwrap_or(Value::Null));
        }
        // Internal connection values and ordinary synthesized objects carry their fields directly.
        if ty.ends_with("Connection")
            || ty.ends_with("Edge")
            || matches!(
                ty,
                "PageInfo"
                    | "RateLimit"
                    | "PullRequestChangedFile"
                    | "PullRequestCommit"
                    | "CreateIssuePayload"
                    | "CloseIssuePayload"
                    | "ReopenIssuePayload"
                    | "UpdateIssuePayload"
                    | "AddCommentPayload"
                    | "CreatePullRequestPayload"
                    | "MergePullRequestPayload"
                    | "ClosePullRequestPayload"
                    | "CreateRepositoryPayload"
            )
        {
            return Ok(p.get(name).cloned().unwrap_or(Value::Null));
        }
        let rid = p.get("_repo").and_then(Value::as_u64).unwrap_or(0);
        let repo = self.forge.repo_by_id(rid);
        match ty {
            "User" => Ok(match name {
                "id" => json!(gid("U", &self.forge.user.id.to_string())),
                "databaseId" => json!(self.forge.user.id),
                "login" => json!(self.forge.user.login),
                "name" => json!(self.forge.user.name),
                "email" => json!(""),
                "url" => {
                    json!(format!("{}/{}", self.origin, self.forge.user.login))
                }
                "avatarUrl" => json!(format!("{}/avatars/{}", self.origin, self.forge.user.login)),
                _ => Value::Null,
            }),
            "Repository" => {
                let Some(r) = repo else {
                    return Ok(Value::Null);
                };
                Ok(match name{"id"=>json!(gid("R",&r.id.to_string())),"databaseId"=>json!(r.id),"name"=>json!(r.name),"nameWithOwner"=>json!(format!("{}/{}",self.forge.login,r.name)),"owner"=>user(),"description"=>r.description.map(Value::String).unwrap_or(Value::Null),"url"=>json!(format!("{}/{}/{}",self.origin,self.forge.login,r.name)),"sshUrl"=>json!(format!("{}/{}/{}.git",self.origin,self.forge.login,r.name)),"isPrivate"=>json!(r.is_private),"visibility"=>json!(if r.is_private{"PRIVATE"}else{"PUBLIC"}),"isFork"|"isArchived"=>json!(false),"isEmpty"=>json!(r.refs.is_empty()),"createdAt"=>json!(r.created_at),"updatedAt"=>json!(r.updated_at),"pushedAt"=>r.pushed_at.map(Value::String).unwrap_or(Value::Null),"stargazerCount"|"forkCount"=>json!(0),"viewerPermission"=>json!("ADMIN"),"viewerCanAdminister"=>json!(true),"hasIssuesEnabled"=>json!(r.has_issues),"hasWikiEnabled"=>json!(false),"mergeCommitAllowed"|"squashMergeAllowed"|"rebaseMergeAllowed"=>json!(true),"defaultBranchRef"=>r.branch_tip(&r.default_branch).map(|oid|cv("Ref",json!({"_repo":r.id,"_ref":format!("refs/heads/{}",r.default_branch),"_oid":oid}))).unwrap_or(Value::Null),"ref"=>{let q=jstr(a.get("qualifiedName").unwrap_or(&Value::Null));let full=if q.starts_with("refs/"){q}else{format!("refs/heads/{q}")};r.refs.get(&full).map(|oid|cv("Ref",json!({"_repo":r.id,"_ref":full,"_oid":oid}))).unwrap_or(Value::Null)},"issue"=>r.issues.get(&a.get("number").and_then(Value::as_u64).unwrap_or(0)).map(|i|issue_val(&r,i)).unwrap_or(Value::Null),"issueOrPullRequest"=>{let no=a.get("number").and_then(Value::as_u64).unwrap_or(0);r.issues.get(&no).map(|i|issue_val(&r,i)).or_else(||r.pulls.get(&no).map(|x|pull_val(&r,x))).unwrap_or(Value::Null)},"issues"=>{let states=a.get("states").and_then(Value::as_array);let labels=a.get("labels").and_then(Value::as_array);let filter_states=a.get("filterBy").and_then(|x|x.get("states")).and_then(Value::as_array);let mut is:Vec<Issue>=r.issues.values().filter(|i|states.is_none_or(|s|s.is_empty()||s.iter().any(|x|x.as_str()==Some(issue_state(i))))&&filter_states.is_none_or(|s|s.is_empty()||s.iter().any(|x|x.as_str()==Some(issue_state(i))))&&labels.is_none_or(|ls|ls.is_empty()||i.labels.iter().any(|l|ls.iter().any(|x|x.as_str().is_some_and(|s|s.eq_ignore_ascii_case(l)))))).cloned().collect();sort_issues(&mut is,a.get("orderBy").unwrap_or(&Value::Null));connection("IssueConnection",is.iter().map(|i|issue_val(&r,i)).collect(),a).map_err(|m|(422,m))?},"pullRequest"=>r.pulls.get(&a.get("number").and_then(Value::as_u64).unwrap_or(0)).map(|x|pull_val(&r,x)).unwrap_or(Value::Null),"pullRequests"=>{let states=a.get("states").and_then(Value::as_array);let ps:Vec<Pull>=r.pulls.values().filter(|x|states.is_none_or(|s|s.is_empty()||s.iter().any(|v|v.as_str()==Some(pull_state(x))))).cloned().collect();connection("PullRequestConnection",ps.iter().map(|x|pull_val(&r,x)).collect(),a).map_err(|m|(422,m))?},"label"=>r.labels.get(&jstr(a.get("name").unwrap_or(&Value::Null)).to_lowercase()).map(|l|cv("Label",json!({"_repo":r.id,"_label":l.id}))).unwrap_or(Value::Null),"labels"=>{let mut ls=store::Forge::list_labels(&self.forge,&r);if let Some(q)=a.get("query").and_then(Value::as_str){ls.retain(|l|l.name.to_lowercase().contains(&q.to_lowercase()));}connection("LabelConnection",ls.iter().map(|l|cv("Label",json!({"_repo":r.id,"_label":l.id}))).collect(),a).map_err(|m|(422,m))?},"refs"=>{let prefix=a.get("refPrefix").and_then(Value::as_str).unwrap_or("refs/");let mut refs:Vec<_>=r.refs.iter().filter(|(k,_)|k.starts_with(prefix)).collect();refs.sort_by_key(|(k,_)|*k);connection("RefConnection",refs.iter().map(|(k,v)|cv("Ref",json!({"_repo":r.id,"_ref":k,"_oid":v}))).collect(),a).map_err(|m|(422,m))?},_=>Value::Null})
            }
            "Issue" => {
                let no = p.get("_number").and_then(Value::as_u64).unwrap_or(0);
                let Some(r) = repo else {
                    return Ok(Value::Null);
                };
                let Some(i) = r.issues.get(&no) else {
                    return Ok(Value::Null);
                };
                Ok(match name {
                    "id" => json!(gid("I", &i.id.to_string())),
                    "databaseId" => json!(i.id),
                    "number" => json!(i.number),
                    "title" => json!(i.title),
                    "body" | "bodyText" => json!(i.body),
                    "state" => json!(issue_state(i)),
                    "stateReason" => i
                        .state_reason
                        .as_ref()
                        .map(|s| json!(s.to_uppercase()))
                        .unwrap_or(Value::Null),
                    "closed" => json!(i.state == "closed"),
                    "closedAt" => i
                        .closed_at
                        .clone()
                        .map(Value::String)
                        .unwrap_or(Value::Null),
                    "createdAt" => json!(i.created_at),
                    "updatedAt" => json!(i.updated_at),
                    "locked" => json!(false),
                    "url" => json!(format!(
                        "{}/{}/{}/issues/{}",
                        self.origin, self.forge.login, r.name, i.number
                    )),
                    "author" => user(),
                    "repository" => repo_val(&r),
                    "labels" => connection(
                        "LabelConnection",
                        i.labels
                            .iter()
                            .filter_map(|n| r.labels.get(&n.to_lowercase()))
                            .map(|l| cv("Label", json!({"_repo":r.id,"_label":l.id})))
                            .collect(),
                        a,
                    )
                    .map_err(|m| (422, m))?,
                    "comments" => connection(
                        "IssueCommentConnection",
                        i.comments
                            .iter()
                            .map(|c| {
                                cv(
                                    "IssueComment",
                                    json!({"_repo":r.id,"_number":i.number,"_comment":c.id}),
                                )
                            })
                            .collect(),
                        a,
                    )
                    .map_err(|m| (422, m))?,
                    "assignees" => connection("UserConnection", vec![], a).map_err(|m| (422, m))?,
                    _ => Value::Null,
                })
            }
            "PullRequest" => {
                let no = p.get("_number").and_then(Value::as_u64).unwrap_or(0);
                let Some(r) = repo else {
                    return Ok(Value::Null);
                };
                let Some(pr) = r.pulls.get(&no) else {
                    return Ok(Value::Null);
                };
                Ok(match name{"id"=>json!(gid("PR",&pr.id.to_string())),"databaseId"=>json!(pr.id),"number"=>json!(pr.number),"title"=>json!(pr.title),"body"=>json!(pr.body),"state"=>json!(pull_state(pr)),"merged"=>json!(pr.merged),"mergedAt"=>pr.merged_at.clone().map(Value::String).unwrap_or(Value::Null),"closed"=>json!(pr.state=="closed"),"closedAt"=>pr.closed_at.clone().map(Value::String).unwrap_or(Value::Null),"createdAt"=>json!(pr.created_at),"updatedAt"=>json!(pr.updated_at),"isDraft"=>json!(false),"url"=>json!(format!("{}/{}/{}/pull/{}",self.origin,self.forge.login,r.name,pr.number)),"author"=>user(),"repository"=>repo_val(&r),"headRefName"=>json!(pr.head),"baseRefName"=>json!(pr.base),"headRefOid"=>json!(self.forge.pull_tips(&r,pr).0),"baseRefOid"=>json!(self.forge.pull_tips(&r,pr).1),"headRef"=>r.branch_tip(&pr.head).map(|oid|cv("Ref",json!({"_repo":r.id,"_ref":format!("refs/heads/{}",pr.head),"_oid":oid}))).unwrap_or(Value::Null),"baseRef"=>r.branch_tip(&pr.base).map(|oid|cv("Ref",json!({"_repo":r.id,"_ref":format!("refs/heads/{}",pr.base),"_oid":oid}))).unwrap_or(Value::Null),"mergeable"=>json!(if self.forge.mergeable(&r,pr){"MERGEABLE"}else{"CONFLICTING"}),"mergeCommit"=>pr.merge_commit_sha.as_ref().map(|oid|cv("Commit",json!({"_repo":r.id,"_oid":oid}))).unwrap_or(Value::Null),"additions"|"deletions"|"changedFiles"=>{let fs=self.forge.pull_files(&r,pr).unwrap_or_default();json!(match name{"additions"=>fs.iter().map(|x|x.additions).sum::<usize>(),"deletions"=>fs.iter().map(|x|x.deletions).sum::<usize>(),_=>fs.len()})},"files"=>{let fs=self.forge.pull_files(&r,pr).unwrap_or_default();connection("PullRequestChangedFileConnection",fs.iter().map(|f|json!({"__typename":"PullRequestChangedFile","path":f.path,"additions":f.additions,"deletions":f.deletions,"changeType":match f.status.as_str(){"added"=>"ADDED","removed"=>"DELETED",_=>"MODIFIED"}})).collect(),a).map_err(|m|(422,m))?},"commits"=>connection("PullRequestCommitConnection",self.forge.pull_commits(&r,pr).iter().map(|oid|json!({"__typename":"PullRequestCommit","commit":{"__typename":"Commit","_repo":r.id,"_oid":oid}})).collect(),a).map_err(|m|(422,m))?,"comments"=>connection("IssueCommentConnection",pr.comments.iter().map(|c|cv("IssueComment",json!({"_repo":r.id,"_number":pr.number,"_comment":c.id}))).collect(),a).map_err(|m|(422,m))?,"labels"=>connection("LabelConnection",vec![],a).map_err(|m|(422,m))?,_=>Value::Null})
            }
            "Label" => {
                let Some(r) = repo else {
                    return Ok(Value::Null);
                };
                let id = p.get("_label").and_then(Value::as_u64).unwrap_or(0);
                let Some(l) = r.labels.values().find(|x| x.id == id) else {
                    return Ok(Value::Null);
                };
                Ok(match name {
                    "id" => json!(gid("LA", &l.id.to_string())),
                    "name" => json!(l.name),
                    "color" => json!(l.color),
                    "description" => l
                        .description
                        .clone()
                        .map(Value::String)
                        .unwrap_or(Value::Null),
                    _ => Value::Null,
                })
            }
            "IssueComment" => {
                let Some(r) = repo else {
                    return Ok(Value::Null);
                };
                let id = p.get("_comment").and_then(Value::as_u64).unwrap_or(0);
                let no = p.get("_number").and_then(Value::as_u64).unwrap_or(0);
                let c = r
                    .issues
                    .get(&no)
                    .and_then(|i| i.comments.iter().find(|c| c.id == id))
                    .or_else(|| {
                        r.pulls
                            .get(&no)
                            .and_then(|i| i.comments.iter().find(|c| c.id == id))
                    });
                let Some(c) = c else { return Ok(Value::Null) };
                Ok(match name {
                    "id" => json!(gid("IC", &c.id.to_string())),
                    "databaseId" => json!(c.id),
                    "body" => json!(c.body),
                    "createdAt" => json!(c.created_at),
                    "updatedAt" => json!(c.updated_at),
                    "url" => json!(format!(
                        "{}/{}/{}/issues/{}#issuecomment-{}",
                        self.origin, self.forge.login, r.name, no, c.id
                    )),
                    "author" => user(),
                    _ => Value::Null,
                })
            }
            "Ref" => {
                let Some(r) = repo else {
                    return Ok(Value::Null);
                };
                let reference = jstr(p.get("_ref").unwrap_or(&Value::Null));
                let oid = jstr(p.get("_oid").unwrap_or(&Value::Null));
                Ok(match name {
                    "id" => json!(gid("REF", &format!("{}:{reference}", r.id))),
                    "name" => json!(reference
                        .strip_prefix("refs/heads/")
                        .or_else(|| reference.strip_prefix("refs/tags/"))
                        .unwrap_or(&reference)),
                    "prefix" => json!(if reference.starts_with("refs/heads/") {
                        "refs/heads/"
                    } else if reference.starts_with("refs/tags/") {
                        "refs/tags/"
                    } else {
                        "refs/"
                    }),
                    "target" => cv("Commit", json!({"_repo":r.id,"_oid":oid})),
                    "repository" => repo_val(&r),
                    _ => Value::Null,
                })
            }
            "Commit" => {
                let Some(r) = repo else {
                    return Ok(Value::Null);
                };
                let oid = jstr(p.get("_oid").unwrap_or(&Value::Null));
                let commit = self.forge.commit_of(&r, &oid);
                Ok(match name {
                    "id" => json!(gid("C", &oid)),
                    "oid" => json!(oid),
                    "abbreviatedOid" => json!(oid.chars().take(7).collect::<String>()),
                    "message" => json!(commit
                        .as_ref()
                        .map(|c| c.message.trim_end_matches('\n'))
                        .unwrap_or("")),
                    "messageHeadline" => json!(commit
                        .as_ref()
                        .and_then(|c| c.message.lines().next())
                        .unwrap_or("")),
                    "committedDate" | "authoredDate" => json!(store::now_iso()),
                    "url" => json!(format!(
                        "{}/{}/{}/commit/{}",
                        self.origin, self.forge.login, r.name, oid
                    )),
                    _ => Value::Null,
                })
            }
            _ => Ok(p.get(name).cloned().unwrap_or(Value::Null)),
        }
    }
    fn mutation(&mut self, name: &str, a: &Map<String, Value>) -> Result<Value, (u16, String)> {
        let input = a
            .get("input")
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();
        let get = |k: &str| input.get(k).cloned().unwrap_or(Value::Null);
        let client = get("clientMutationId");
        match name {
            "createIssue" => {
                let repo = self.repo_arg(&jstr(&get("repositoryId")))?;
                let labels: Vec<String> = get("labelIds")
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|id| node_from_id(self.forge, &jstr(id)))
                    .filter_map(|n| {
                        let id = n.get("_label")?.as_u64()?;
                        let rr = self.forge.repo_by_id(n.get("_repo")?.as_u64()?)?;
                        rr.labels
                            .values()
                            .find(|x| x.id == id)
                            .map(|x| x.name.clone())
                    })
                    .collect();
                let mut r = repo;
                let issue = self
                    .forge
                    .create_issue(
                        &mut r,
                        &jstr(&get("title")),
                        get("body").as_str().unwrap_or(""),
                        &labels,
                    )
                    .map_err(api_err)?;
                Ok(
                    json!({"__typename":"CreateIssuePayload","issue":issue_val(&r,&issue),"clientMutationId":client}),
                )
            }
            "closeIssue" | "reopenIssue" | "updateIssue" => {
                let id = if name == "updateIssue" {
                    get("id")
                } else {
                    get("issueId")
                };
                let node = node_from_id(self.forge, &jstr(&id))
                    .filter(|n| n.get("__typename").and_then(Value::as_str) == Some("Issue"))
                    .ok_or_else(|| {
                        (
                            404,
                            format!(
                                "Could not resolve to a node with the global id of '{}'",
                                jstr(&id)
                            ),
                        )
                    })?;
                let mut r = self.repo_node_arg(&node)?;
                let no = node.get("_number").and_then(Value::as_u64).unwrap_or(0);
                let mut patch = store::IssueUpdate::default();
                if name == "closeIssue" {
                    patch.state = Some("closed".into());
                    patch.state_reason = Some(
                        get("stateReason")
                            .as_str()
                            .unwrap_or("COMPLETED")
                            .to_lowercase(),
                    );
                } else if name == "reopenIssue" {
                    patch.state = Some("open".into());
                } else {
                    patch.title = get("title").as_str().map(str::to_string);
                    patch.body = get("body").as_str().map(str::to_string);
                    patch.state = get("state")
                        .as_str()
                        .filter(|x| *x == "OPEN" || *x == "CLOSED")
                        .map(|x| x.to_lowercase());
                }
                self.forge
                    .update_issue(&mut r, no, patch)
                    .map_err(api_err)?;
                let payload = if name == "closeIssue" {
                    "CloseIssuePayload"
                } else if name == "reopenIssue" {
                    "ReopenIssuePayload"
                } else {
                    "UpdateIssuePayload"
                };
                Ok(json!({"__typename":payload,"issue":node,"clientMutationId":client}))
            }
            "createPullRequest" => {
                let mut r = self.repo_arg(&jstr(&get("repositoryId")))?;
                let pr = self
                    .forge
                    .create_pull(
                        &mut r,
                        &jstr(&get("title")),
                        get("body").as_str().unwrap_or(""),
                        &jstr(&get("headRefName")),
                        &jstr(&get("baseRefName")),
                    )
                    .map_err(api_err)?;
                Ok(
                    json!({"__typename":"CreatePullRequestPayload","pullRequest":pull_val(&r,&pr),"clientMutationId":client}),
                )
            }
            "mergePullRequest" | "closePullRequest" => {
                let id = get("pullRequestId");
                let node = node_from_id(self.forge, &jstr(&id))
                    .filter(|n| n.get("__typename").and_then(Value::as_str) == Some("PullRequest"))
                    .ok_or_else(|| {
                        (
                            404,
                            format!(
                                "Could not resolve to a node with the global id of '{}'",
                                jstr(&id)
                            ),
                        )
                    })?;
                let mut r = self.repo_node_arg(&node)?;
                let no = node.get("_number").and_then(Value::as_u64).unwrap_or(0);
                if name == "mergePullRequest" {
                    let method = get("mergeMethod")
                        .as_str()
                        .unwrap_or("MERGE")
                        .to_lowercase();
                    self.forge
                        .merge_pull(
                            &mut r,
                            no,
                            store::MergeOptions {
                                method: Some(method),
                                title: get("commitHeadline").as_str().map(str::to_string),
                                message: get("commitBody").as_str().map(str::to_string),
                                sha: get("expectedHeadOid").as_str().map(str::to_string),
                            },
                        )
                        .map_err(api_err)?;
                } else {
                    self.forge
                        .update_pull(
                            &mut r,
                            no,
                            store::PullUpdate {
                                state: Some("closed".into()),
                                ..Default::default()
                            },
                        )
                        .map_err(api_err)?;
                }
                Ok(
                    json!({"__typename":if name=="mergePullRequest"{"MergePullRequestPayload"}else{"ClosePullRequestPayload"},"pullRequest":node,"clientMutationId":client}),
                )
            }
            "addComment" => {
                let id = get("subjectId");
                let node = node_from_id(self.forge, &jstr(&id))
                    .filter(|n| {
                        matches!(
                            n.get("__typename").and_then(Value::as_str),
                            Some("Issue" | "PullRequest")
                        )
                    })
                    .ok_or_else(|| {
                        (
                            404,
                            format!(
                                "Could not resolve to a node with the global id of '{}'",
                                jstr(&id)
                            ),
                        )
                    })?;
                let mut r = self.repo_node_arg(&node)?;
                let no = node.get("_number").and_then(Value::as_u64).unwrap_or(0);
                let comment = self
                    .forge
                    .add_comment(&mut r, no, get("body").as_str().unwrap_or(""))
                    .map_err(api_err)?;
                let count = r
                    .issues
                    .get(&no)
                    .map(|i| i.comments.len())
                    .or_else(|| r.pulls.get(&no).map(|x| x.comments.len()))
                    .unwrap_or(1);
                let c = cv(
                    "IssueComment",
                    json!({"_repo":r.id,"_number":no,"_comment":comment.id}),
                );
                let edge = json!({"__typename":"IssueCommentEdge","node":c,"cursor":enc64(format!("cursor:v2:{}",count-1).as_bytes())});
                Ok(
                    json!({"__typename":"AddCommentPayload","commentEdge":edge,"subject":node,"clientMutationId":client}),
                )
            }
            "createRepository" => {
                let name = jstr(&get("name"));
                let r = self
                    .forge
                    .create_repo(
                        name,
                        get("description").as_str().map(str::to_string),
                        get("visibility") == "PRIVATE",
                        true,
                    )
                    .map_err(api_err)?;
                Ok(
                    json!({"__typename":"CreateRepositoryPayload","repository":repo_val(&r),"clientMutationId":client}),
                )
            }
            _ => Ok(Value::Null),
        }
    }
    fn repo_arg(&self, id: &str) -> Result<Repo, (u16, String)> {
        let (prefix, key) = parse_gid(id).ok_or_else(|| {
            (
                404,
                format!("Could not resolve to a node with the global id of '{id}'"),
            )
        })?;
        let repo = if prefix == "R" {
            key.parse::<u64>()
                .ok()
                .and_then(|n| self.forge.repo_by_id(n))
        } else {
            None
        };
        repo.ok_or_else(|| {
            (
                404,
                format!("Could not resolve to a node with the global id of '{id}'"),
            )
        })
    }
    fn repo_node_arg(&self, node: &Value) -> Result<Repo, (u16, String)> {
        let id = node.get("_repo").and_then(Value::as_u64).unwrap_or(0);
        self.forge.repo_by_id(id).ok_or_else(|| {
            (
                404,
                format!(
                    "Could not resolve to a node with the global id of '{}'",
                    node.get("id").map(jstr).unwrap_or_default()
                ),
            )
        })
    }
}
fn implements(runtime: &str, condition: &str) -> bool {
    runtime == condition
        || matches!(
            (runtime, condition),
            (
                "User" | "Repository" | "Issue" | "IssueComment" | "Label" | "PullRequest" | "Ref",
                "Node"
            ) | ("Commit", "Node" | "GitObject")
        )
}
fn input_value(name: &str, ty: &str) -> Value {
    json!({"__typename":"__InputValue","name":name,"description":null,"type":type_ref(ty),"defaultValue":null})
}

fn run_one(forge: &mut Forge, origin: &str, request: &Value) -> Value {
    let Some(obj) = request.as_object() else {
        return json!({"errors":[{"message":"A query attribute must be specified and must be a string."}]});
    };
    let Some(query) = obj
        .get("query")
        .and_then(Value::as_str)
        .filter(|x| !x.trim().is_empty())
    else {
        return json!({"errors":[{"message":"A query attribute must be specified and must be a string."}]});
    };
    let defs = match Parser::new(query).and_then(|mut p| p.document()) {
        Ok(v) => v,
        Err(e) => {
            return json!({"errors":[{"message":e.message,"locations":[{"line":e.line,"column":e.col}]}]})
        }
    };
    let mut ops = vec![];
    let mut frags = HashMap::new();
    for d in defs {
        match d {
            Def::Op(o) => ops.push(o),
            Def::Fragment(n, f) => {
                frags.insert(n, f);
            }
        }
    }
    let op_name = obj
        .get("operationName")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty());
    let op = if let Some(n) = op_name {
        match ops.iter().find(|o| o.name.as_deref() == Some(n)) {
            Some(o) => o.clone(),
            None => {
                return json!({"errors":[{"message":format!("Unknown operation named \"{n}\".")}]})
            }
        }
    } else if ops.len() == 1 {
        ops[0].clone()
    } else {
        return json!({"errors":[{"message":if ops.is_empty(){"No operations in query document"}else{"An operation name is required"}}]});
    };
    if op.kind == "subscription" {
        return json!({"errors":[{"message":"Subscriptions are not supported"}]});
    }
    let given = obj.get("variables").and_then(Value::as_object);
    let mut vars = Map::new();
    let mut var_errors = vec![];
    for d in &op.vars {
        if let Some(v) = given.and_then(|g| g.get(&d.name)) {
            vars.insert(d.name.clone(), v.clone());
        } else if let Some(default) = &d.default {
            let empty = Exec {
                forge,
                origin,
                variables: Map::new(),
                frags: &frags,
                errors: vec![],
            };
            vars.insert(d.name.clone(), empty.eval(default));
        } else if d.ty.ends_with('!') {
            var_errors.push(json!({"message":format!("Variable ${} of type {} was provided invalid value",d.name,d.ty),"extensions":{"code":"variableMismatch","variableName":d.name,"typeName":d.ty}}));
        } else {
            vars.insert(d.name.clone(), Value::Null);
        }
    }
    if !var_errors.is_empty() {
        return json!({"errors":var_errors});
    }
    let root = if op.kind == "mutation" {
        "Mutation"
    } else {
        "Query"
    };
    let mut validation = vec![];
    validate(&op.sels, root, &frags, &mut HashSet::new(), &mut validation);
    if !validation.is_empty() {
        return json!({"errors":validation});
    }
    let mut exec = Exec {
        forge,
        origin,
        variables: vars,
        frags: &frags,
        errors: vec![],
    };
    let data = exec.selection(&op.sels, root, &json!({}), &[]);
    if exec.errors.is_empty() {
        json!({"data":data})
    } else {
        json!({"data":data,"errors":exec.errors})
    }
}

pub fn handle(forge: &mut Forge, request: &Request, origin: &str) -> Response {
    let value: Value = match serde_json::from_slice(&request.body) {
        Ok(v) => v,
        Err(_) => return Response::json_error(400, "Problems parsing JSON"),
    };
    let answer = if let Some(batch) = value.as_array() {
        Value::Array(
            batch
                .iter()
                .map(|item| run_one(forge, origin, item))
                .collect(),
        )
    } else {
        run_one(forge, origin, &value)
    };
    Response::send_json(200, &answer.to_string())
}
