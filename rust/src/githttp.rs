//! Git smart-HTTP protocol plumbing. Object encoding and pack parsing live in `git`; ref and object
//! mutations are committed through the shared `Forge` store.

use crate::git::{self, GitObject, ObjType, Packet, ZERO_OID};
use crate::http::{Request, Response};
use crate::store::{Forge, ObjectType, Repo};
use std::collections::{HashMap, HashSet, VecDeque};

pub const AGENT: &str = "gitforge/0.1";

const UPLOAD_V0_CAPS: &[&str] = &[
    "multi_ack_detailed",
    "side-band-64k",
    "thin-pack",
    "ofs-delta",
    "shallow",
    "no-progress",
    "allow-tip-sha1-in-want",
    "allow-reachable-sha1-in-want",
];
const RECEIVE_CAPS: &[&str] = &[
    "report-status",
    "delete-refs",
    "side-band-64k",
    "quiet",
    "ofs-delta",
    "push-options",
];

fn response(status: u16, content_type: &str, body: Vec<u8>) -> Response {
    Response {
        status,
        body,
        content_type: content_type.to_string(),
        extra: vec![("cache-control".to_string(), "no-cache".to_string())],
    }
}

fn plain(status: u16, message: &str) -> Response {
    response(
        status,
        "text/plain; charset=utf-8",
        format!("{message}\n").into_bytes(),
    )
}

fn pkt_text(value: &str) -> Vec<u8> {
    git::pkt(value.as_bytes())
}

fn joined(parts: impl IntoIterator<Item = Vec<u8>>) -> Vec<u8> {
    parts.into_iter().flatten().collect()
}

fn protocol_v2(value: &str) -> bool {
    value.split(':').any(|part| part.trim() == "version=2")
}

fn sorted_refs(repo: &Repo) -> Vec<(String, String)> {
    let mut refs: Vec<_> = repo
        .refs
        .iter()
        .map(|(name, oid)| (name.clone(), oid.clone()))
        .collect();
    refs.sort_by(|left, right| left.0.cmp(&right.0));
    refs
}

fn emit_v0_ref(
    packets: &mut Vec<Vec<u8>>,
    first: &mut bool,
    capabilities: &str,
    oid: &str,
    name: &str,
) {
    let line = if *first {
        *first = false;
        format!("{oid} {name}\0{capabilities}\n")
    } else {
        format!("{oid} {name}\n")
    };
    packets.push(pkt_text(&line));
}

fn v0_advertisement(forge: &Forge, repo: &Repo, caps: &[&str]) -> Vec<Vec<u8>> {
    let mut packets = Vec::new();
    let head_ref = forge.head_ref(repo);
    let head_oid = repo.refs.get(&head_ref);
    let capabilities = format!(
        "{} symref=HEAD:{head_ref} object-format=sha1 agent={AGENT}",
        caps.join(" ")
    );
    let mut first = true;
    if let Some(oid) = head_oid {
        emit_v0_ref(&mut packets, &mut first, &capabilities, oid, "HEAD");
    }
    for (name, oid) in sorted_refs(repo) {
        emit_v0_ref(&mut packets, &mut first, &capabilities, &oid, &name);
        if name.starts_with("refs/tags/") {
            let peeled = forge.peel(repo, &oid);
            if peeled != oid {
                packets.push(pkt_text(&format!("{peeled} {name}^{{}}\n")));
            }
        }
    }
    if first {
        emit_v0_ref(
            &mut packets,
            &mut first,
            &capabilities,
            ZERO_OID,
            "capabilities^{}",
        );
    }
    packets
}

pub fn info_refs(forge: &Forge, repo: &Repo, service: &str, git_protocol: &str) -> Response {
    match service {
        "git-upload-pack" => {
            let mut body = Vec::new();
            if protocol_v2(git_protocol) {
                for line in [
                    "version 2\n",
                    "agent=gitforge/0.1\n",
                    "ls-refs=unborn\n",
                    "fetch=shallow\n",
                    "server-option\n",
                    "object-format=sha1\n",
                ] {
                    body.extend_from_slice(&pkt_text(line));
                }
                body.extend_from_slice(&git::flush());
            } else {
                body.extend_from_slice(&pkt_text("# service=git-upload-pack\n"));
                body.extend_from_slice(&git::flush());
                for packet in v0_advertisement(forge, repo, UPLOAD_V0_CAPS) {
                    body.extend_from_slice(&packet);
                }
                body.extend_from_slice(&git::flush());
            }
            response(200, "application/x-git-upload-pack-advertisement", body)
        }
        "git-receive-pack" => {
            let mut body = pkt_text("# service=git-receive-pack\n");
            body.extend_from_slice(&git::flush());
            for packet in v0_advertisement(forge, repo, RECEIVE_CAPS) {
                body.extend_from_slice(&packet);
            }
            body.extend_from_slice(&git::flush());
            response(200, "application/x-git-receive-pack-advertisement", body)
        }
        _ => plain(
            403,
            "smart HTTP serves git-upload-pack and git-receive-pack only",
        ),
    }
}

#[derive(Default)]
struct FetchRequest {
    wants: Vec<String>,
    haves: Vec<String>,
    shallow: HashSet<String>,
    depth: Option<usize>,
    done: bool,
}

struct PackPlan {
    oids: Vec<String>,
    shallow: Vec<String>,
    unshallow: Vec<String>,
}

fn store_object(object: &crate::store::GitObject) -> GitObject {
    GitObject {
        kind: match object.object_type {
            ObjectType::Commit => ObjType::Commit,
            ObjectType::Tree => ObjType::Tree,
            ObjectType::Blob => ObjType::Blob,
            ObjectType::Tag => ObjType::Tag,
        },
        data: object.data.clone(),
    }
}

fn plan_pack(
    forge: &Forge,
    repo: &Repo,
    wants: &[String],
    haves: &[String],
    client_shallow: &HashSet<String>,
    depth: Option<usize>,
) -> PackPlan {
    let mut have = HashSet::new();
    let mut have_stack: Vec<String> = haves
        .iter()
        .filter(|oid| repo.objects.contains_key(*oid))
        .cloned()
        .collect();
    while let Some(oid) = have_stack.pop() {
        if !have.insert(oid.clone()) {
            continue;
        }
        let Some(object) = repo.get(&oid) else {
            continue;
        };
        match object.object_type {
            ObjectType::Commit => {
                let commit = git::parse_commit(&object.data);
                have_stack.push(commit.tree);
                if !client_shallow.contains(&oid) {
                    have_stack.extend(commit.parents);
                }
            }
            ObjectType::Tree => {
                if let Ok(entries) = git::parse_tree(&object.data) {
                    have_stack.extend(
                        entries
                            .into_iter()
                            .filter(|entry| entry.mode != "160000")
                            .map(|entry| entry.oid),
                    );
                }
            }
            ObjectType::Tag => have_stack.push(git::tag_target(&object.data).0),
            ObjectType::Blob => {}
        }
    }

    let mut commits = Vec::new();
    let mut extra = Vec::new();
    let mut commit_depth: HashMap<String, usize> = HashMap::new();
    let mut shallow = HashSet::new();
    let mut queue = VecDeque::new();
    for want in wants {
        let mut oid = want.clone();
        for _ in 0..10 {
            let Some(object) = repo.get(&oid) else { break };
            if object.object_type != ObjectType::Tag {
                break;
            }
            if !have.contains(&oid) {
                extra.push(oid.clone());
            }
            oid = git::tag_target(&object.data).0;
        }
        match repo.get(&oid) {
            Some(object) if object.object_type == ObjectType::Commit => {
                queue.push_back((oid, 1usize))
            }
            Some(_) if !have.contains(&oid) => extra.push(oid),
            _ => {}
        }
    }
    while let Some((oid, level)) = queue.pop_front() {
        if commit_depth.get(&oid).is_some_and(|known| *known <= level) {
            continue;
        }
        let first_visit = !commit_depth.contains_key(&oid);
        commit_depth.insert(oid.clone(), level);
        if depth.is_none() && have.contains(&oid) {
            continue;
        }
        let Some(commit) = forge.commit_of(repo, &oid) else {
            continue;
        };
        if first_visit && !have.contains(&oid) {
            commits.push(oid.clone());
        }
        if depth.is_some_and(|limit| level >= limit) {
            if !commit.parents.is_empty() {
                shallow.insert(oid);
            }
            continue;
        }
        shallow.remove(&oid);
        for parent in commit.parents {
            queue.push_back((parent, level + 1));
        }
    }

    let mut unshallow = Vec::new();
    if let Some(depth) = depth {
        for oid in client_shallow {
            if commit_depth.get(oid).is_some_and(|level| *level < depth)
                && forge
                    .commit_of(repo, oid)
                    .is_some_and(|commit| !commit.parents.is_empty())
            {
                unshallow.push(oid.clone());
            }
        }
    }
    unshallow.sort();
    let mut new_shallow: Vec<_> = shallow
        .into_iter()
        .filter(|oid| !client_shallow.contains(oid))
        .collect();
    new_shallow.sort();

    let mut send = Vec::new();
    let mut queued = HashSet::new();
    let add = |oid: &str, send: &mut Vec<String>, queued: &mut HashSet<String>| {
        if !queued.contains(oid) && !have.contains(oid) && repo.objects.contains_key(oid) {
            queued.insert(oid.to_string());
            send.push(oid.to_string());
        }
    };
    for oid in &extra {
        add(oid, &mut send, &mut queued);
    }
    for oid in &commits {
        add(oid, &mut send, &mut queued);
    }
    let mut tree_stack = Vec::new();
    for oid in &commits {
        if let Some(commit) = forge.commit_of(repo, oid) {
            tree_stack.push(commit.tree);
        }
    }
    while let Some(oid) = tree_stack.pop() {
        if queued.contains(&oid) || have.contains(&oid) {
            continue;
        }
        let Some(object) = repo.get(&oid) else {
            continue;
        };
        add(&oid, &mut send, &mut queued);
        if object.object_type == ObjectType::Tree {
            if let Ok(entries) = git::parse_tree(&object.data) {
                for entry in entries {
                    if entry.mode == "160000" {
                        continue;
                    }
                    if entry.mode == "40000" {
                        tree_stack.push(entry.oid);
                    } else {
                        add(&entry.oid, &mut send, &mut queued);
                    }
                }
            }
        }
    }
    PackPlan {
        oids: send,
        shallow: new_shallow,
        unshallow,
    }
}

fn build_pack(repo: &mut Repo, oids: &[String]) -> Result<Vec<u8>, String> {
    let mut entries = Vec::with_capacity(oids.len());
    for oid in oids {
        entries.push(repo.entry_for(oid)?);
    }
    Ok(git::write_pack(&entries))
}


/// The pack bytes for one fetch plan: built once per distinct object list and then shared, so the
/// fifty concurrent clones of a benchmark share one buffer instead of rebuilding it under the lock.
fn plan_pack_bytes(repo: &mut Repo, plan: &PackPlan) -> Result<std::sync::Arc<Vec<u8>>, String> {
    let mut key = String::with_capacity(64 + plan.oids.len() * 41);
    key.push_str(&plan.oids.len().to_string());
    key.push(':');
    for oid in &plan.oids {
        key.push_str(oid);
        key.push(',');
    }
    if let Some(hit) = repo.served_packs.get(&key) {
        return Ok(hit.clone());
    }
    let pack = build_pack(repo, &plan.oids)?;
    let shared = std::sync::Arc::new(pack);
    repo.served_packs.insert(key, shared.clone());
    Ok(shared)
}

fn invalid_want(repo: &Repo, wants: &[String]) -> Option<String> {
    wants
        .iter()
        .find(|oid| !git::is_oid(oid) || !repo.objects.contains_key(*oid))
        .cloned()
}

fn ls_refs(forge: &Forge, repo: &Repo, args: &[String]) -> Vec<u8> {
    let peel = args.iter().any(|arg| arg == "peel");
    let symrefs = args.iter().any(|arg| arg == "symrefs");
    let unborn = args.iter().any(|arg| arg == "unborn");
    let prefixes: Vec<_> = args
        .iter()
        .filter_map(|arg| arg.strip_prefix("ref-prefix "))
        .collect();
    let keep =
        |name: &str| prefixes.is_empty() || prefixes.iter().any(|prefix| name.starts_with(prefix));
    let head_ref = forge.head_ref(repo);
    let mut packets = Vec::new();
    if keep("HEAD") {
        if let Some(oid) = repo.refs.get(&head_ref) {
            let suffix = if symrefs {
                format!(" symref-target:{head_ref}")
            } else {
                String::new()
            };
            packets.push(pkt_text(&format!("{oid} HEAD{suffix}\n")));
        } else if unborn {
            let suffix = if symrefs {
                format!(" symref-target:{head_ref}")
            } else {
                String::new()
            };
            packets.push(pkt_text(&format!("unborn HEAD{suffix}\n")));
        }
    }
    for (name, oid) in sorted_refs(repo) {
        if !keep(&name) {
            continue;
        }
        let peeled = if peel {
            forge.peel(repo, &oid)
        } else {
            oid.clone()
        };
        let suffix = if peeled != oid {
            format!(" peeled:{peeled}")
        } else {
            String::new()
        };
        packets.push(pkt_text(&format!("{oid} {name}{suffix}\n")));
    }
    packets.push(git::flush());
    joined(packets)
}

fn parse_v2_request(packets: &[Packet]) -> (String, Vec<String>) {
    let mut command = String::new();
    let mut args = Vec::new();
    let mut in_args = false;
    for packet in packets {
        match packet {
            Packet::Flush => break,
            Packet::Delim => in_args = true,
            Packet::Data(_) if in_args => args.push(git::line_of(packet)),
            Packet::Data(_) => {
                let line = git::line_of(packet);
                if let Some(value) = line.strip_prefix("command=") {
                    command = value.to_string();
                }
            }
            Packet::End => {}
        }
    }
    (command, args)
}

fn parse_fetch_args(args: &[String]) -> FetchRequest {
    let mut request = FetchRequest::default();
    for arg in args {
        if let Some(value) = arg.strip_prefix("want ") {
            request.wants.push(value.trim().to_string());
        } else if let Some(value) = arg.strip_prefix("have ") {
            request.haves.push(value.trim().to_string());
        } else if let Some(value) = arg.strip_prefix("shallow ") {
            request.shallow.insert(value.trim().to_string());
        } else if let Some(value) = arg.strip_prefix("deepen ") {
            request.depth = Some(value.trim().parse::<usize>().unwrap_or(1).max(1));
        } else if arg == "done" {
            request.done = true;
        }
    }
    request
}

fn fetch_v2(forge: &Forge, repo: &mut Repo, args: &[String]) -> Response {
    let request = parse_fetch_args(args);
    if let Some(bad) = invalid_want(repo, &request.wants) {
        return response(
            200,
            "application/x-git-upload-pack-result",
            joined([
                pkt_text(&format!("ERR upload-pack: not our ref {bad}\n")),
                git::flush(),
            ]),
        );
    }
    let common: Vec<_> = request
        .haves
        .iter()
        .filter(|oid| repo.objects.contains_key(*oid))
        .cloned()
        .collect();
    let mut packets = Vec::new();
    if !request.done {
        packets.push(pkt_text("acknowledgments\n"));
        if common.is_empty() {
            packets.push(pkt_text("NAK\n"));
        }
        for oid in &common {
            packets.push(pkt_text(&format!("ACK {oid}\n")));
        }
        if request.wants.is_empty() {
            packets.push(git::flush());
            return response(200, "application/x-git-upload-pack-result", joined(packets));
        }
        packets.push(pkt_text("ready\n"));
        packets.push(git::delim());
    }
    let plan = plan_pack(
        forge,
        repo,
        &request.wants,
        &common,
        &request.shallow,
        request.depth,
    );
    if request.depth.is_some() || !request.shallow.is_empty() {
        packets.push(pkt_text("shallow-info\n"));
        packets.extend(
            plan.shallow
                .iter()
                .map(|oid| pkt_text(&format!("shallow {oid}\n"))),
        );
        packets.extend(
            plan.unshallow
                .iter()
                .map(|oid| pkt_text(&format!("unshallow {oid}\n"))),
        );
        packets.push(git::delim());
    }
    packets.push(pkt_text("packfile\n"));
    match plan_pack_bytes(repo, &plan) {
        Ok(pack) => packets.extend(git::sideband(1, &pack)),
        Err(error) => {
            return response(
                200,
                "application/x-git-upload-pack-result",
                joined([pkt_text(&format!("ERR {error}\n")), git::flush()]),
            )
        }
    }
    packets.push(git::flush());
    response(200, "application/x-git-upload-pack-result", joined(packets))
}

fn upload_v2(forge: &Forge, repo: &mut Repo, packets: &[Packet]) -> Response {
    let (command, args) = parse_v2_request(packets);
    if command == "ls-refs" {
        return response(
            200,
            "application/x-git-upload-pack-result",
            ls_refs(forge, repo, &args),
        );
    }
    if command == "fetch" {
        return fetch_v2(forge, repo, &args);
    }
    response(
        200,
        "application/x-git-upload-pack-result",
        joined([
            pkt_text(&format!("ERR unknown command {command:?}\n")),
            git::flush(),
        ]),
    )
}

fn upload_v0(forge: &Forge, repo: &mut Repo, packets: &[Packet]) -> Response {
    let mut request = FetchRequest::default();
    let mut caps = HashSet::new();
    for packet in packets {
        let Packet::Data(_) = packet else { continue };
        let line = git::line_of(packet);
        if let Some(rest) = line.strip_prefix("want ") {
            let split: Vec<_> = rest.split(' ').collect();
            request
                .wants
                .push(split.first().copied().unwrap_or("").to_string());
            if split.len() > 1 && caps.is_empty() {
                caps.extend(split[1..].iter().map(|cap| cap.to_string()));
            }
        } else if let Some(value) = line.strip_prefix("have ") {
            request.haves.push(value.trim().to_string());
        } else if let Some(value) = line.strip_prefix("shallow ") {
            request.shallow.insert(value.trim().to_string());
        } else if let Some(value) = line.strip_prefix("deepen ") {
            request.depth = Some(value.trim().parse::<usize>().unwrap_or(1).max(1));
        } else if line == "done" {
            request.done = true;
        }
    }
    if let Some(bad) = invalid_want(repo, &request.wants) {
        return response(
            200,
            "application/x-git-upload-pack-result",
            joined([
                pkt_text(&format!("ERR upload-pack: not our ref {bad}\n")),
                git::flush(),
            ]),
        );
    }
    let common: Vec<_> = request
        .haves
        .iter()
        .filter(|oid| repo.objects.contains_key(*oid))
        .cloned()
        .collect();
    let multi_ack = caps.contains("multi_ack_detailed") || caps.contains("multi_ack");
    let mut packets_out = Vec::new();
    if !request.done && !request.haves.is_empty() {
        if multi_ack {
            packets_out.extend(
                common
                    .iter()
                    .map(|oid| pkt_text(&format!("ACK {oid} common\n"))),
            );
        }
        packets_out.push(pkt_text("NAK\n"));
        return response(
            200,
            "application/x-git-upload-pack-result",
            joined(packets_out),
        );
    }
    let plan = plan_pack(
        forge,
        repo,
        &request.wants,
        &common,
        &request.shallow,
        request.depth,
    );
    if request.depth.is_some() || !request.shallow.is_empty() {
        packets_out.extend(
            plan.shallow
                .iter()
                .map(|oid| pkt_text(&format!("shallow {oid}\n"))),
        );
        packets_out.extend(
            plan.unshallow
                .iter()
                .map(|oid| pkt_text(&format!("unshallow {oid}\n"))),
        );
        packets_out.push(git::flush());
    }
    if common.is_empty() {
        packets_out.push(pkt_text("NAK\n"));
    } else if multi_ack {
        packets_out.extend(
            common
                .iter()
                .map(|oid| pkt_text(&format!("ACK {oid} common\n"))),
        );
        packets_out.push(pkt_text(&format!("ACK {}\n", common[common.len() - 1])));
    } else {
        packets_out.push(pkt_text(&format!("ACK {}\n", common[0])));
    }
    match plan_pack_bytes(repo, &plan) {
        Ok(pack) if caps.contains("side-band-64k") || caps.contains("side-band") => {
            packets_out.extend(git::sideband(1, &pack));
            packets_out.push(git::flush());
        }
        Ok(pack) => packets_out.push(pack.to_vec()),
        Err(error) => {
            return response(
                200,
                "application/x-git-upload-pack-result",
                joined([pkt_text(&format!("ERR {error}\n")), git::flush()]),
            )
        }
    }
    response(
        200,
        "application/x-git-upload-pack-result",
        joined(packets_out),
    )
}

pub fn upload_pack(forge: &Forge, repo: &mut Repo, body: &[u8], git_protocol: &str) -> Response {
    let packets = match git::read_packets(body, 0, false) {
        Ok((packets, _)) => packets,
        Err(error) => return plain(400, &format!("malformed upload-pack request: {error}")),
    };
    let is_v2 = protocol_v2(git_protocol)
        || packets
            .iter()
            .find_map(|packet| match packet {
                Packet::Data(_) => Some(git::line_of(packet).starts_with("command=")),
                _ => None,
            })
            .unwrap_or(false);
    if is_v2 {
        upload_v2(forge, repo, &packets)
    } else {
        upload_v0(forge, repo, &packets)
    }
}

#[derive(Clone)]
struct PushCommand {
    old_oid: String,
    new_oid: String,
    reference: String,
}

fn valid_ref_name(reference: &str) -> bool {
    if !reference.starts_with("refs/")
        || reference.ends_with('/')
        || reference.ends_with(".lock")
        || reference.ends_with('.')
    {
        return false;
    }
    let bytes = reference.as_bytes();
    if bytes
        .windows(2)
        .any(|window| window == b".." || window == b"//" || window == b"@{" || window == b"/.")
    {
        return false;
    }
    !bytes.iter().any(|byte| {
        *byte <= 0x20
            || *byte == 0x7f
            || matches!(*byte, b'~' | b'^' | b':' | b'?' | b'*' | b'[' | b'\\')
    })
}

fn connected(repo: &Repo, tip: &str, incoming: &HashMap<String, GitObject>) -> bool {
    let mut seen = HashSet::new();
    let mut stack = vec![tip.to_string()];
    while let Some(oid) = stack.pop() {
        if !seen.insert(oid.clone()) {
            continue;
        }
        let Some(object) = repo
            .get(&oid)
            .map(store_object)
            .or_else(|| incoming.get(&oid).cloned())
        else {
            return false;
        };
        if repo.objects.contains_key(&oid) && !incoming.contains_key(&oid) {
            continue;
        }
        match object.kind {
            ObjType::Commit => {
                let commit = git::parse_commit(&object.data);
                stack.push(commit.tree);
                stack.extend(commit.parents);
            }
            ObjType::Tree => {
                let Ok(entries) = git::parse_tree(&object.data) else {
                    return false;
                };
                stack.extend(
                    entries
                        .into_iter()
                        .filter(|entry| entry.mode != "160000")
                        .map(|entry| entry.oid),
                );
            }
            ObjType::Tag => stack.push(git::tag_target(&object.data).0),
            ObjType::Blob => {}
        }
    }
    true
}

pub fn receive_pack(forge: &mut Forge, repo: &mut Repo, body: &[u8]) -> Response {
    let (packets, next) = match git::read_packets(body, 0, true) {
        Ok(result) => result,
        Err(error) => return plain(400, &format!("malformed receive-pack request: {error}")),
    };
    let mut commands = Vec::new();
    let mut caps = HashSet::new();
    for packet in &packets {
        let Packet::Data(_) = packet else { continue };
        let mut line = git::line_of(packet);
        if let Some(nul) = line.find('\0') {
            if commands.is_empty() {
                caps.extend(
                    line[nul + 1..]
                        .split(' ')
                        .filter(|value| !value.is_empty())
                        .map(str::to_string),
                );
            }
            line.truncate(nul);
        }
        if line.starts_with("shallow ") {
            continue;
        }
        let parts: Vec<_> = line.split(' ').collect();
        if parts.len() != 3 || !git::is_oid(parts[0]) || !git::is_oid(parts[1]) {
            return plain(400, &format!("malformed receive-pack command {line:?}"));
        }
        commands.push(PushCommand {
            old_oid: parts[0].to_string(),
            new_oid: parts[1].to_string(),
            reference: parts[2].to_string(),
        });
    }
    let pack = &body[next..];
    let mut unpack = "ok".to_string();
    let mut incoming = HashMap::new();
    if !pack.is_empty() {
        match git::read_pack(pack, &|oid| repo.get(oid).map(store_object)) {
            Ok(objects) => incoming = objects,
            Err(error) => unpack = error,
        }
    }
    if unpack == "ok" {
        forge.put_objects(repo, &incoming);
    }

    let mut results = Vec::new();
    let mut created_branches = Vec::new();
    for command in commands {
        let PushCommand {
            old_oid,
            new_oid,
            reference,
        } = command;
        if unpack != "ok" {
            results.push(format!("ng {reference} unpacker error"));
            continue;
        }
        if !valid_ref_name(&reference) {
            results.push(format!("ng {reference} funny refname"));
            continue;
        }
        let current = repo.refs.get(&reference).cloned();
        if new_oid == ZERO_OID {
            match current {
                None => results.push(format!("ng {reference} reference does not exist")),
                Some(current) if old_oid != ZERO_OID && old_oid != current => {
                    results.push(format!("ng {reference} stale info"));
                }
                Some(_) => {
                    forge.delete_ref(repo, &reference);
                    results.push(format!("ok {reference}"));
                }
            }
            continue;
        }
        if !repo.objects.contains_key(&new_oid) || !connected(repo, &new_oid, &incoming) {
            results.push(format!("ng {reference} missing necessary objects"));
            continue;
        }
        let Some(current) = current else {
            if old_oid != ZERO_OID {
                results.push(format!("ng {reference} stale info"));
                continue;
            }
            if reference.starts_with("refs/heads/")
                && repo
                    .get(&new_oid)
                    .is_none_or(|object| object.object_type != ObjectType::Commit)
            {
                results.push(format!("ng {reference} not a commit"));
                continue;
            }
            forge.write_ref(repo, &reference, &new_oid);
            if let Some(branch) = reference.strip_prefix("refs/heads/") {
                created_branches.push(branch.to_string());
            }
            results.push(format!("ok {reference}"));
            continue;
        };
        if old_oid != current {
            let fast_forward = repo
                .get(&new_oid)
                .is_some_and(|object| object.object_type == ObjectType::Commit)
                && forge.is_ancestor(repo, &current, &new_oid);
            results.push(if fast_forward {
                format!("ng {reference} stale info")
            } else {
                format!("ng {reference} non-fast-forward")
            });
            continue;
        }
        let old_commit = forge.peel(repo, &current);
        let new_commit = forge.peel(repo, &new_oid);
        let both_commits = repo
            .get(&old_commit)
            .is_some_and(|object| object.object_type == ObjectType::Commit)
            && repo
                .get(&new_commit)
                .is_some_and(|object| object.object_type == ObjectType::Commit);
        if reference.starts_with("refs/tags/")
            || !both_commits
            || !forge.is_ancestor(repo, &old_commit, &new_commit)
        {
            results.push(format!("ng {reference} non-fast-forward"));
            continue;
        }
        forge.write_ref(repo, &reference, &new_oid);
        results.push(format!("ok {reference}"));
    }
    forge.adopt_default_branch(repo, &created_branches);

    if !caps.contains("report-status") && !caps.contains("report-status-v2") {
        return response(200, "application/x-git-receive-pack-result", Vec::new());
    }
    let mut report = pkt_text(&format!(
        "unpack {}\n",
        if unpack == "ok" {
            "ok".to_string()
        } else {
            unpack.replace('\n', " ")
        }
    ));
    for line in results {
        report.extend_from_slice(&pkt_text(&format!("{line}\n")));
    }
    report.extend_from_slice(&git::flush());
    if caps.contains("side-band-64k") || caps.contains("side-band") {
        let mut body = joined(git::sideband(1, &report));
        body.extend_from_slice(&git::flush());
        response(200, "application/x-git-receive-pack-result", body)
    } else {
        response(200, "application/x-git-receive-pack-result", report)
    }
}

pub fn missing_repo() -> Response {
    plain(404, "Repository not found")
}

pub fn auth_error(message: &str) -> Response {
    let mut result = plain(401, message);
    result.extra.push((
        "www-authenticate".to_string(),
        "Bearer realm=\"gitforge\"".to_string(),
    ));
    result
}

pub fn request_git_protocol(request: &Request) -> &str {
    request
        .headers
        .get("git-protocol")
        .map(String::as_str)
        .unwrap_or("")
}
