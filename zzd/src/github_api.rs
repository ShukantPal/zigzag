//! Small authenticated GitHub API client shared by PR, comment, and review flows.

use base64::Engine;
use serde_json::Value;
#[cfg(test)]
use std::cell::RefCell;
use std::io::Read;
use std::path::PathBuf;

const API_ROOT: &str = "https://api.github.com";
const MAX_PAGES: usize = 1000;
const MAX_REDIRECTS: usize = 5;
const GITHUB_BUNDLE_HOSTS: &[&str] = &[
    "api.github.com",
    "github.com",
    "objects.githubusercontent.com",
    "release-assets.githubusercontent.com",
    "tmaproduction.blob.core.windows.net",
];
const KEYCHAIN_BASE64_PREFIX: &str = "go-keyring-base64:";

fn unwrap_keychain_token(raw: &str) -> Option<String> {
    let raw = raw.trim();
    match raw.strip_prefix(KEYCHAIN_BASE64_PREFIX) {
        Some(encoded) => base64::engine::general_purpose::STANDARD
            .decode(encoded)
            .ok()
            .and_then(|decoded| String::from_utf8(decoded).ok())
            .map(|token| token.trim().to_owned()),
        None => Some(raw.to_owned()),
    }
}

#[cfg(test)]
thread_local! {
    static API_ROOT_OVERRIDE: RefCell<Option<String>> = const { RefCell::new(None) };
}

fn api_root() -> String {
    #[cfg(test)]
    if let Some(root) = API_ROOT_OVERRIDE.with(|root| root.borrow().clone()) {
        return root;
    }

    API_ROOT.to_owned()
}

fn github_token() -> Result<String, String> {
    github_token_with_sources(
        std::env::var("GH_TOKEN").ok(),
        std::env::var("GITHUB_TOKEN").ok(),
        std::env::var_os("GH_CONFIG_DIR")
            .map(PathBuf::from)
            .or_else(|| {
                std::env::var_os("XDG_CONFIG_HOME").map(|path| PathBuf::from(path).join("gh"))
            })
            .or_else(|| {
                std::env::var_os("HOME").map(|path| PathBuf::from(path).join(".config/gh"))
            }),
    )
}

fn github_token_with_sources(
    gh_token: Option<String>,
    github_token: Option<String>,
    config: Option<PathBuf>,
) -> Result<String, String> {
    github_token_from_env(gh_token, github_token, config)
}

fn github_token_from_env(
    gh_token: Option<String>,
    github_token: Option<String>,
    config: Option<PathBuf>,
) -> Result<String, String> {
    github_token_from_env_with_keychain(gh_token, github_token, config, |account| {
        keyring::Entry::new("gh:github.com", account)
            .ok()
            .and_then(|entry| entry.get_password().ok())
    })
}

fn github_token_from_env_with_keychain(
    gh_token: Option<String>,
    github_token: Option<String>,
    config: Option<PathBuf>,
    mut get_password: impl FnMut(&str) -> Option<String>,
) -> Result<String, String> {
    for token in [gh_token, github_token].into_iter().flatten() {
        if !token.trim().is_empty() {
            return Ok(token);
        }
    }

    // Resolve the same host configuration used by the GitHub CLI. Its secure
    // credential-store entry is keyed by the active username (and it keeps an
    // unkeyed active-account entry for older configurations).
    let config = config.ok_or_else(|| {
        "GitHub authentication is unavailable (set GH_TOKEN or GITHUB_TOKEN)".to_owned()
    })?;
    let contents = std::fs::read_to_string(config.join("hosts.yml")).unwrap_or_default();
    let hosts: serde_yaml::Value =
        serde_yaml::from_str(&contents).unwrap_or(serde_yaml::Value::Null);
    let host = hosts.get("github.com").and_then(|host| {
        host.as_sequence()
            .and_then(|accounts| accounts.first())
            .or_else(|| host.as_mapping().map(|_| host))
    });
    let user = host
        .and_then(|account| account.get("user"))
        .and_then(serde_yaml::Value::as_str);
    if let Some(user) = user
        && let Some(token) = get_password(user).and_then(|token| unwrap_keychain_token(&token))
        && !token.is_empty()
    {
        return Ok(token);
    }
    if let Some(token) = get_password("").and_then(|token| unwrap_keychain_token(&token))
        && !token.is_empty()
    {
        return Ok(token);
    }
    let token = host
        .and_then(|account| account.get("oauth_token"))
        .and_then(serde_yaml::Value::as_str)
        .filter(|token| !token.trim().is_empty())
        .ok_or_else(|| {
            "GitHub authentication is unavailable (set GH_TOKEN or GITHUB_TOKEN)".to_owned()
        })?;
    Ok(token.to_owned())
}

fn agent() -> Result<(ureq::Agent, String), String> {
    let token = github_token()?;
    Ok((build_agent(), token))
}

fn build_agent() -> ureq::Agent {
    ureq::AgentBuilder::new()
        .user_agent(concat!("zigzag/", env!("CARGO_PKG_VERSION")))
        .redirects(0)
        .build()
}

fn headers(request: ureq::Request, token: &str) -> ureq::Request {
    request
        .set("Authorization", &format!("Bearer {token}"))
        .set("Accept", "application/vnd.github+json")
        .set("X-GitHub-Api-Version", "2022-11-28")
}

fn response_json(response: Result<ureq::Response, ureq::Error>) -> Result<Value, String> {
    let response = match response {
        Ok(response) => response,
        // Do not include the response body or transport error here. Both are
        // untrusted and can echo request credentials; callers may log this
        // error, which would persist a GitHub token in cleartext.
        Err(ureq::Error::Status(code, _)) => {
            return Err(format!("GitHub API returned HTTP {code}"));
        }
        Err(ureq::Error::Transport(_)) => return Err("GitHub API request failed".to_owned()),
    };
    response
        .into_json()
        .map_err(|_| "GitHub API returned invalid JSON".to_owned())
}

pub(crate) fn get(endpoint: &str) -> Result<Value, String> {
    get_at_root(&api_root(), endpoint)
}

fn get_at_root(api_root: &str, endpoint: &str) -> Result<Value, String> {
    let (agent, token) = agent()?;
    get_at(&agent, api_root, endpoint, &token)
}

fn get_at(
    agent: &ureq::Agent,
    api_root: &str,
    endpoint: &str,
    token: &str,
) -> Result<Value, String> {
    response_json(headers(agent.get(&format!("{api_root}/{endpoint}")), token).call())
}

/// Fetch every page from a REST collection endpoint. GitHub's maximum page
/// size is 100; page numbers are added without changing the endpoint filters.
pub(crate) fn get_all(endpoint: &str) -> Result<Vec<Value>, String> {
    let (agent, token) = agent()?;
    get_all_at(&agent, API_ROOT, endpoint, &token)
}

fn get_all_at(
    agent: &ureq::Agent,
    api_root: &str,
    endpoint: &str,
    token: &str,
) -> Result<Vec<Value>, String> {
    let mut results = Vec::new();
    for page in 1..=MAX_PAGES {
        let separator = if endpoint.contains('?') { '&' } else { '?' };
        let url = format!("{api_root}/{endpoint}{separator}per_page=100&page={page}");
        let value = response_json(headers(agent.get(&url), token).call())?;
        let items = value
            .as_array()
            .ok_or_else(|| "GitHub API returned an unexpected collection".to_owned())?;
        results.extend(items.iter().cloned());
        if items.len() < 100 {
            return Ok(results);
        }
    }
    Err("GitHub API pagination exceeded 1000 pages".to_owned())
}

pub(crate) fn post(endpoint: &str, body: &Value) -> Result<Value, String> {
    let (agent, token) = agent()?;
    post_at(&agent, API_ROOT, endpoint, body, &token)
}

fn post_at(
    agent: &ureq::Agent,
    api_root: &str,
    endpoint: &str,
    body: &Value,
    token: &str,
) -> Result<Value, String> {
    response_json(
        headers(agent.post(&format!("{api_root}/{endpoint}")), token).send_json(body.clone()),
    )
}

pub(crate) fn delete(endpoint: &str) -> Result<(), String> {
    let (agent, token) = agent()?;
    delete_at(&agent, API_ROOT, endpoint, &token)
}

fn delete_at(
    agent: &ureq::Agent,
    api_root: &str,
    endpoint: &str,
    token: &str,
) -> Result<(), String> {
    match headers(agent.delete(&format!("{api_root}/{endpoint}")), token).call() {
        Ok(_) => Ok(()),
        Err(ureq::Error::Status(204, _)) => Ok(()),
        // Keep authenticated response and transport details out of errors:
        // callers may write them to persistent logs.
        Err(ureq::Error::Status(code, _)) => Err(format!("GitHub API returned HTTP {code}")),
        Err(ureq::Error::Transport(_)) => Err("GitHub API request failed".to_owned()),
    }
}

pub(crate) fn attestation_bundles(repository: &str, digest: &str) -> Result<Vec<Vec<u8>>, String> {
    let (agent, token) = agent()?;
    attestation_bundles_at(&agent, API_ROOT, repository, digest, &token)
}

fn attestation_bundles_at(
    agent: &ureq::Agent,
    api_root: &str,
    repository: &str,
    digest: &str,
    token: &str,
) -> Result<Vec<Vec<u8>>, String> {
    let attestations = attestation_metadata_at(agent, api_root, repository, digest, token)?;
    if attestations.is_empty() {
        return Err("GitHub returned no artifact attestations".to_owned());
    }
    attestations
        .iter()
        .map(|attestation| {
            let url = attestation
                .get("bundle_url")
                .and_then(Value::as_str)
                .ok_or_else(|| "GitHub attestation is missing its bundle URL".to_owned())?;
            validate_bundle_url(url, GITHUB_BUNDLE_HOSTS)?;
            let response = get_bundle(url)?;
            let mut bytes = Vec::new();
            response
                .into_reader()
                .take(32 * 1024 * 1024)
                .read_to_end(&mut bytes)
                .map_err(|error| format!("could not read GitHub attestation bundle: {error}"))?;
            decode_snappy_bundle(&bytes)
        })
        .collect()
}

fn attestation_metadata_at(
    agent: &ureq::Agent,
    api_root: &str,
    repository: &str,
    digest: &str,
    token: &str,
) -> Result<Vec<Value>, String> {
    let api_origin = url::Url::parse(api_root)
        .map_err(|_| "GitHub API returned an invalid pagination URL".to_owned())?;
    let mut next = Some(format!(
        "{api_root}/repos/{repository}/attestations/sha256:{digest}?per_page=100"
    ));
    let mut results = Vec::new();
    for _ in 0..MAX_PAGES {
        let Some(url) = next.take() else {
            return Ok(results);
        };
        if !same_origin(&api_origin, &url) {
            return Err("GitHub API returned an unsafe pagination URL".to_owned());
        }
        let response = headers(agent.get(&url), token).call();
        let response = match response {
            Ok(response) => response,
            Err(ureq::Error::Status(code, _)) => {
                return Err(format!("GitHub API returned HTTP {code}"));
            }
            Err(ureq::Error::Transport(_)) => return Err("GitHub API request failed".to_owned()),
        };
        let link = response.header("Link").map(str::to_owned);
        let value: Value = response
            .into_json()
            .map_err(|_| "GitHub API returned invalid JSON".to_owned())?;
        let items = value
            .get("attestations")
            .and_then(Value::as_array)
            .ok_or_else(|| "GitHub did not return artifact attestations".to_owned())?;
        results.extend(items.iter().cloned());
        next = link.as_deref().and_then(next_link);
    }
    Err("GitHub API pagination exceeded 1000 pages".to_owned())
}

fn next_link(header: &str) -> Option<String> {
    header.split(',').find_map(|part| {
        let mut pieces = part.trim().split(';');
        let url = pieces.next()?.trim().strip_prefix('<')?.strip_suffix('>')?;
        pieces
            .any(|parameter| parameter.trim().eq_ignore_ascii_case("rel=\"next\""))
            .then(|| url.to_owned())
    })
}

fn same_origin(origin: &url::Url, candidate: &str) -> bool {
    let Ok(candidate) = url::Url::parse(candidate) else {
        return false;
    };
    origin.scheme() == candidate.scheme()
        && origin.host_str() == candidate.host_str()
        && origin.port_or_known_default() == candidate.port_or_known_default()
        && candidate.username().is_empty()
        && candidate.password().is_none()
}

fn validate_bundle_url(url: &str, allowed_hosts: &[&str]) -> Result<url::Url, String> {
    let parsed =
        url::Url::parse(url).map_err(|_| "GitHub attestation bundle URL is unsafe".to_owned())?;
    let host = parsed.host_str().unwrap_or_default().to_ascii_lowercase();
    if parsed.scheme() != "https"
        || parsed.port_or_known_default() != Some(443)
        || parsed.username() != ""
        || parsed.password().is_some()
        || (!allowed_hosts.iter().any(|allowed| host == *allowed) && !is_github_blob_host(&host))
    {
        return Err("GitHub attestation bundle URL is unsafe".to_owned());
    }
    Ok(parsed)
}

fn is_github_blob_host(host: &str) -> bool {
    if host == "tmastaging.blob.core.windows.net" {
        return true;
    }
    let Some(account) = host.strip_suffix(".blob.core.windows.net") else {
        return false;
    };
    let Some(number) = account.strip_prefix("productionresultssa") else {
        return false;
    };
    (1..=2).contains(&number.len()) && number.bytes().all(|byte| byte.is_ascii_digit())
}

fn get_bundle(url: &str) -> Result<ureq::Response, String> {
    let agent = ureq::AgentBuilder::new().redirects(0).build();
    let mut current = url.to_owned();
    for _ in 0..=MAX_REDIRECTS {
        validate_bundle_url(&current, GITHUB_BUNDLE_HOSTS)?;
        let result = agent.get(&current).call();
        match result {
            Ok(response) => return Ok(response),
            Err(ureq::Error::Status(301 | 302 | 303 | 307 | 308, response)) => {
                let location = response
                    .header("Location")
                    .ok_or_else(|| "could not download GitHub attestation bundle".to_owned())?;
                current = bundle_redirect_target(&current, location)?;
            }
            Err(_) => return Err("could not download GitHub attestation bundle".to_owned()),
        }
    }
    Err("could not download GitHub attestation bundle".to_owned())
}

fn bundle_redirect_target(current: &str, location: &str) -> Result<String, String> {
    let base = url::Url::parse(current)
        .map_err(|_| "GitHub attestation bundle URL is unsafe".to_owned())?;
    let target = base
        .join(location)
        .map_err(|_| "GitHub attestation bundle URL is unsafe".to_owned())?
        .to_string();
    validate_bundle_url(&target, GITHUB_BUNDLE_HOSTS)?;
    Ok(target)
}

fn decode_snappy_bundle(bytes: &[u8]) -> Result<Vec<u8>, String> {
    const IDENTIFIER: &[u8] = b"\xff\x06\x00\x00sNaPpY";
    if bytes.first().is_some_and(|byte| *byte == b'{') {
        return Ok(bytes.to_vec());
    }
    if !bytes.starts_with(IDENTIFIER) {
        let decoded_length = snap::raw::decompress_len(bytes)
            .map_err(|_| "GitHub attestation bundle has an unsupported encoding".to_owned())?;
        if decoded_length > 32 * 1024 * 1024 {
            return Err("GitHub attestation bundle exceeded its size limit".to_owned());
        }
        return snap::raw::Decoder::new()
            .decompress_vec(bytes)
            .map_err(|_| "GitHub attestation bundle has an invalid Snappy block".to_owned());
    }
    let mut output = Vec::new();
    let mut position = IDENTIFIER.len();
    while position < bytes.len() {
        if position + 4 > bytes.len() {
            return Err("GitHub attestation bundle is truncated".to_owned());
        }
        let kind = bytes[position];
        let length = bytes[position + 1] as usize
            | ((bytes[position + 2] as usize) << 8)
            | ((bytes[position + 3] as usize) << 16);
        position += 4;
        let end = position
            .checked_add(length)
            .filter(|end| *end <= bytes.len())
            .ok_or_else(|| "GitHub attestation bundle is truncated".to_owned())?;
        let chunk = &bytes[position..end];
        match kind {
            0x00 if chunk.len() >= 4 => {
                let decoded = decode_snappy_block(&chunk[4..])?;
                check_snappy_crc(&chunk[..4], &decoded)?;
                output.extend(decoded);
            }
            0x01 if chunk.len() >= 4 => {
                check_snappy_crc(&chunk[..4], &chunk[4..])?;
                output.extend_from_slice(&chunk[4..]);
            }
            0x00 | 0x01 => return Err("GitHub attestation bundle chunk is truncated".to_owned()),
            0xff if chunk == b"sNaPpY" => {}
            0x80..=0xfe => {}
            _ => return Err("GitHub attestation bundle has an invalid chunk".to_owned()),
        }
        if output.len() > 32 * 1024 * 1024 {
            return Err("GitHub attestation bundle exceeded its size limit".to_owned());
        }
        position = end;
    }
    if output.is_empty() {
        return Err("GitHub attestation bundle was empty".to_owned());
    }
    Ok(output)
}

fn check_snappy_crc(encoded_crc: &[u8], contents: &[u8]) -> Result<(), String> {
    let expected = u32::from_le_bytes(encoded_crc.try_into().unwrap());
    let mut crc = !0u32;
    for byte in contents {
        crc ^= *byte as u32;
        for _ in 0..8 {
            crc = (crc >> 1) ^ (0x82f63b78 & (0u32.wrapping_sub(crc & 1)));
        }
    }
    let masked = (!crc).rotate_right(15).wrapping_add(0xa282ead8);
    if masked == expected {
        Ok(())
    } else {
        Err("GitHub attestation bundle checksum failed".to_owned())
    }
}

fn decode_snappy_block(block: &[u8]) -> Result<Vec<u8>, String> {
    let mut position = 0usize;
    let mut expected = 0usize;
    let mut shift = 0u32;
    loop {
        let byte = *block
            .get(position)
            .ok_or_else(|| "GitHub attestation block is truncated".to_owned())?;
        position += 1;
        expected |= ((byte & 0x7f) as usize)
            .checked_shl(shift)
            .ok_or_else(|| "GitHub attestation block is too large".to_owned())?;
        if byte & 0x80 == 0 {
            break;
        }
        shift += 7;
        if shift > 28 {
            return Err("GitHub attestation block is too large".to_owned());
        }
    }
    if expected > 32 * 1024 * 1024 {
        return Err("GitHub attestation block exceeded its size limit".to_owned());
    }
    let mut output = Vec::with_capacity(expected);
    while position < block.len() {
        let tag = block[position];
        position += 1;
        match tag & 3 {
            0 => {
                let code = (tag >> 2) as usize;
                let length = if code < 60 {
                    code + 1
                } else {
                    let extra = code - 59;
                    if extra > 4 || position + extra > block.len() {
                        return Err("GitHub attestation literal is invalid".to_owned());
                    }
                    let mut size = 0usize;
                    for (index, byte) in block[position..position + extra].iter().enumerate() {
                        size |= (*byte as usize) << (index * 8);
                    }
                    position += extra;
                    size + 1
                };
                let end = position
                    .checked_add(length)
                    .filter(|end| *end <= block.len())
                    .ok_or_else(|| "GitHub attestation literal is truncated".to_owned())?;
                output.extend_from_slice(&block[position..end]);
                position = end;
            }
            kind => {
                let (length, offset) = match kind {
                    1 => {
                        let low = *block
                            .get(position)
                            .ok_or_else(|| "GitHub attestation copy is truncated".to_owned())?;
                        position += 1;
                        (
                            4 + ((tag >> 2) as usize & 7),
                            (((tag as usize) & 0xe0) << 3) | low as usize,
                        )
                    }
                    2 => {
                        if position + 2 > block.len() {
                            return Err("GitHub attestation copy is truncated".to_owned());
                        }
                        let offset =
                            u16::from_le_bytes([block[position], block[position + 1]]) as usize;
                        position += 2;
                        (1 + (tag >> 2) as usize, offset)
                    }
                    _ => {
                        if position + 4 > block.len() {
                            return Err("GitHub attestation copy is truncated".to_owned());
                        }
                        let offset =
                            u32::from_le_bytes(block[position..position + 4].try_into().unwrap())
                                as usize;
                        position += 4;
                        (1 + (tag >> 2) as usize, offset)
                    }
                };
                if offset == 0 || offset > output.len() || output.len() + length > expected {
                    return Err("GitHub attestation copy is invalid".to_owned());
                }
                for _ in 0..length {
                    output.push(output[output.len() - offset]);
                }
            }
        }
        if output.len() > expected {
            return Err("GitHub attestation block length is invalid".to_owned());
        }
    }
    (output.len() == expected)
        .then_some(output)
        .ok_or_else(|| "GitHub attestation block length is invalid".to_owned())
}

pub(crate) fn graphql(query: &str, variables: Value) -> Result<Value, String> {
    let (agent, token) = agent()?;
    graphql_at(&agent, API_ROOT, query, variables, &token)
}

fn graphql_at(
    agent: &ureq::Agent,
    api_root: &str,
    query: &str,
    variables: Value,
    token: &str,
) -> Result<Value, String> {
    let response = post_at(
        agent,
        api_root,
        "graphql",
        &serde_json::json!({"query": query, "variables": variables}),
        token,
    )?;
    if response.get("errors").is_some() {
        // The remote error body is untrusted and can contain sensitive
        // request details; callers may persist this error in logs or status.
        return Err("GitHub GraphQL request failed".to_owned());
    }
    response
        .get("data")
        .cloned()
        .ok_or_else(|| "GitHub GraphQL response is missing data".to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::thread;

    fn read_request(stream: &mut std::net::TcpStream) -> String {
        let mut request = Vec::new();
        let mut byte = [0; 1];
        while !request.ends_with(b"\r\n\r\n") {
            stream.read_exact(&mut byte).unwrap();
            request.push(byte[0]);
        }
        let headers = String::from_utf8(request.clone()).unwrap();
        let content_length = headers
            .lines()
            .find_map(|line| {
                line.to_ascii_lowercase()
                    .strip_prefix("content-length: ")
                    .and_then(|value| value.parse::<usize>().ok())
            })
            .unwrap_or(0);
        let body_start = request.len();
        request.resize(body_start + content_length, 0);
        stream.read_exact(&mut request[body_start..]).unwrap();
        String::from_utf8(request).unwrap()
    }

    fn write_response(stream: &mut std::net::TcpStream, status: &str, headers: &str, body: &str) {
        write!(
            stream,
            "HTTP/1.1 {status}\r\n{headers}Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
        .unwrap();
    }

    #[test]
    fn snappy_bundle_decoder_reads_uncompressed_and_literal_chunks() {
        let identifier = b"\xff\x06\x00\x00sNaPpY";
        let block = [0x02, 0x04, b'o', b'k'];
        let mut frame = identifier.to_vec();
        frame.extend([0x00, (block.len() + 4) as u8, 0, 0]);
        frame.extend(masked_snappy_crc(b"ok").to_le_bytes());
        frame.extend(block);
        assert_eq!(decode_snappy_bundle(&frame).unwrap(), b"ok");

        let mut uncompressed = identifier.to_vec();
        uncompressed.extend([0x01, 0x06, 0, 0]);
        uncompressed.extend(masked_snappy_crc(b"ok").to_le_bytes());
        uncompressed.extend(b"ok");
        assert_eq!(decode_snappy_bundle(&uncompressed).unwrap(), b"ok");
    }

    #[test]
    fn github_snappy_bundle_fixture_verifies_and_tampering_fails() {
        use std::process::Command;

        let fixture_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
        let artifact = std::fs::read(fixture_dir.join("github-attestation-manifest.json")).unwrap();
        let encoded = std::fs::read(fixture_dir.join("github-attestation-bundle.json.sn")).unwrap();
        let decoded = decode_snappy_bundle(&encoded).unwrap();
        let manifest: Value = serde_json::from_slice(&artifact).unwrap();
        let commit = manifest["commit"].as_str().unwrap();
        let root =
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("trust/sigstore-trusted-root.json");
        let temp_dir = std::env::temp_dir().join(format!(
            "zigzag-attestation-fixture-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&temp_dir).unwrap();
        let artifact_path = temp_dir.join("manifest.json");
        let valid_bundle_path = temp_dir.join("bundle.json");
        std::fs::write(&artifact_path, artifact).unwrap();
        std::fs::write(&valid_bundle_path, &decoded).unwrap();

        let verify = |bundle: &std::path::Path| {
            Command::new("gh")
                .args([
                    "attestation",
                    "verify",
                    artifact_path.to_str().unwrap(),
                    "--bundle",
                    bundle.to_str().unwrap(),
                    "--repo",
                    "ShukantPal/zigzag",
                    "--signer-workflow",
                    "ShukantPal/zigzag/.github/workflows/ci.yml",
                    "--source-ref",
                    "refs/heads/main",
                    "--source-digest",
                    commit,
                    "--predicate-type",
                    "https://slsa.dev/provenance/v1",
                    "--custom-trusted-root",
                    root.to_str().unwrap(),
                ])
                .output()
                .expect("GitHub CLI is required to run the attestation fixture test")
        };

        let valid = verify(&valid_bundle_path);
        assert!(
            valid.status.success(),
            "real GitHub attestation fixture did not verify: {}",
            String::from_utf8_lossy(&valid.stderr)
        );

        let mut tampered: Value = serde_json::from_slice(&decoded).unwrap();
        let signature = tampered["dsseEnvelope"]["signatures"][0]["sig"]
            .as_str()
            .unwrap()
            .to_owned();
        let replacement = if signature.starts_with('A') { 'B' } else { 'A' };
        tampered["dsseEnvelope"]["signatures"][0]["sig"] =
            format!("{replacement}{}", &signature[1..]).into();
        let tampered_bundle_path = temp_dir.join("tampered-bundle.json");
        std::fs::write(
            &tampered_bundle_path,
            serde_json::to_vec(&tampered).unwrap(),
        )
        .unwrap();
        let tampered_result = verify(&tampered_bundle_path);
        assert!(tampered_result.status.code().is_some_and(|code| code != 0));
        std::fs::remove_dir_all(temp_dir).unwrap();
    }

    #[test]
    fn public_request_errors_do_not_expose_body_or_token() {
        static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let _env_lock = ENV_LOCK.lock().unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let request = read_request(&mut stream);
            assert!(request.contains("Authorization: Bearer sensitive-test-token\r\n"));
            let body = "Bearer sensitive-test-token";
            write_response(&mut stream, "401 Unauthorized", "", body);
        });

        let previous_token = std::env::var_os("GH_TOKEN");
        unsafe { std::env::set_var("GH_TOKEN", "sensitive-test-token") };
        API_ROOT_OVERRIDE.with(|root| {
            *root.borrow_mut() = Some(format!("http://{address}"));
        });
        let result = get("private");
        API_ROOT_OVERRIDE.with(|root| *root.borrow_mut() = None);
        match previous_token {
            Some(token) => unsafe { std::env::set_var("GH_TOKEN", token) },
            None => unsafe { std::env::remove_var("GH_TOKEN") },
        }

        let error = result.unwrap_err();
        assert_eq!(error, "GitHub API returned HTTP 401");
        assert!(!error.contains("sensitive-test-token"));
        server.join().unwrap();
    }

    #[test]
    fn missing_github_token_and_auth_resolution_fail_safely() {
        let error = github_token_from_env(None, None, None).unwrap_err();
        assert_eq!(
            error,
            "GitHub authentication is unavailable (set GH_TOKEN or GITHUB_TOKEN)"
        );
        assert_eq!(
            github_token_from_env(Some(" test ".into()), None, None).unwrap(),
            " test "
        );
    }

    #[test]
    fn unwrap_keychain_token_decodes_prefixed_value() {
        let expected = "ghp_fakePAT_for_tests_only_decode";
        let encoded = base64::engine::general_purpose::STANDARD.encode(expected);
        let raw = format!("{KEYCHAIN_BASE64_PREFIX}{encoded}");
        assert_eq!(unwrap_keychain_token(&raw).as_deref(), Some(expected));
    }

    #[test]
    fn unwrap_keychain_token_passes_raw_value_through() {
        let raw = "ghp_fakePAT_for_tests_only_raw";
        assert_eq!(unwrap_keychain_token(raw).as_deref(), Some(raw));
    }

    #[test]
    fn unwrap_keychain_token_fails_closed_for_invalid_prefixed_values() {
        let raw = format!("{KEYCHAIN_BASE64_PREFIX}%%%invalid%%%");
        assert_eq!(unwrap_keychain_token(&raw), None);
        let non_utf8 = format!("{KEYCHAIN_BASE64_PREFIX}/w==");
        assert_eq!(unwrap_keychain_token(&non_utf8), None);
    }

    #[test]
    fn unwrap_keychain_token_trims_before_decoding_prefixed_values() {
        let token = "ghp_fakePAT_for_tests_only_whitespace";
        let encoded = base64::engine::general_purpose::STANDARD.encode(token);
        let raw = format!(" \t{KEYCHAIN_BASE64_PREFIX}{encoded}\n ");
        assert_eq!(unwrap_keychain_token(&raw).as_deref(), Some(token));
    }

    #[test]
    fn unwrap_keychain_token_trims_whitespace_and_handles_empty_values() {
        assert_eq!(
            unwrap_keychain_token("  ghp_fakePAT_for_tests_only_trim\n").as_deref(),
            Some("ghp_fakePAT_for_tests_only_trim")
        );
        assert_eq!(unwrap_keychain_token(" \t\n ").as_deref(), Some(""));
        assert_eq!(unwrap_keychain_token("").as_deref(), Some(""));
    }

    fn keychain_test_config(label: &str) -> PathBuf {
        static NEXT_CONFIG: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let id = NEXT_CONFIG.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let config = std::env::temp_dir().join(format!(
            "zigzag-github-keychain-{label}-{}-{id}",
            std::process::id()
        ));
        std::fs::create_dir_all(&config).unwrap();
        std::fs::write(
            config.join("hosts.yml"),
            "github.com:\n  - user: fake-user\n",
        )
        .unwrap();
        config
    }

    #[test]
    fn github_token_resolves_decoded_username_keyed_keychain_value() {
        let config = keychain_test_config("username");
        let token = "ghp_fakePAT_for_tests_only_username";
        let encoded = base64::engine::general_purpose::STANDARD.encode(token);
        let marked = format!("{KEYCHAIN_BASE64_PREFIX}{encoded}");
        let resolved =
            github_token_from_env_with_keychain(None, None, Some(config.clone()), |account| {
                (account == "fake-user").then(|| marked.clone())
            })
            .unwrap();
        assert_eq!(resolved, token);
        std::fs::remove_dir_all(config).unwrap();
    }

    #[test]
    fn github_token_resolves_decoded_legacy_empty_account_keychain_value() {
        let config = keychain_test_config("legacy");
        let token = "ghp_fakePAT_for_tests_only_legacy";
        let encoded = base64::engine::general_purpose::STANDARD.encode(token);
        let marked = format!("{KEYCHAIN_BASE64_PREFIX}{encoded}");
        let resolved =
            github_token_from_env_with_keychain(None, None, Some(config.clone()), |account| {
                (account.is_empty()).then(|| marked.clone())
            })
            .unwrap();
        assert_eq!(resolved, token);
        std::fs::remove_dir_all(config).unwrap();
    }

    fn masked_snappy_crc(contents: &[u8]) -> u32 {
        let mut crc = !0u32;
        for byte in contents {
            crc ^= *byte as u32;
            for _ in 0..8 {
                crc = (crc >> 1) ^ (0x82f63b78 & (0u32.wrapping_sub(crc & 1)));
            }
        }
        (!crc).rotate_right(15).wrapping_add(0xa282ead8)
    }

    #[test]
    fn attestation_api_follows_next_link_pages_before_returning_candidates() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            for page in 1..=2 {
                let (mut stream, _) = listener.accept().unwrap();
                let request = read_request(&mut stream);
                assert!(request.contains("Authorization: Bearer test-token\r\n"));
                if page == 1 {
                    assert!(request.contains("/repos/o/r/attestations/sha256:abc?per_page=100"));
                    let body =
                        r#"{"attestations":[{"bundle_url":"https://api.github.com/bundle/1"}]}"#;
                    write_response(
                        &mut stream,
                        "200 OK",
                        &format!("Link: <http://{address}/page/2>; rel=\"next\"\r\n"),
                        body,
                    );
                } else {
                    assert!(request.contains("/page/2"));
                    let body =
                        r#"{"attestations":[{"bundle_url":"https://api.github.com/bundle/2"}]}"#;
                    write_response(&mut stream, "200 OK", "", body);
                }
            }
        });
        let agent = ureq::AgentBuilder::new().redirects(0).build();
        let candidates = attestation_metadata_at(
            &agent,
            &format!("http://{address}"),
            "o/r",
            "abc",
            "test-token",
        )
        .unwrap();
        assert_eq!(candidates.len(), 2);
        server.join().unwrap();
    }

    #[test]
    fn bundle_url_requires_https_and_an_allowlisted_destination() {
        for unsafe_url in [
            "http://github.com/bundle",
            "https://127.0.0.1/bundle",
            "https://localhost/bundle",
            "https://github.com.attacker.example/bundle",
            "https://user@github.com/bundle",
            "https://anything.blob.core.windows.net/bundle",
            "https://evil.blob.core.windows.net/bundle",
            "https://tmaproduction.evil.com/bundle",
            "https://user:password@tmaproduction.blob.core.windows.net/bundle",
            "https://api.github.com:8443/bundle",
            "https://tmaproduction.blob.core.windows.net:8443/bundle",
            "https://productionresultssa19.blob.core.windows.net:8443/bundle",
        ] {
            assert!(validate_bundle_url(unsafe_url, GITHUB_BUNDLE_HOSTS).is_err());
        }
        assert!(validate_bundle_url("https://api.github.com/bundle", GITHUB_BUNDLE_HOSTS).is_ok());
        assert!(validate_bundle_url(
            "https://tmaproduction.blob.core.windows.net/attestations/1369377226/2026/10/13/55302578.json.sn?sig=redacted",
            GITHUB_BUNDLE_HOSTS,
        )
        .is_ok());
        assert!(
            bundle_redirect_target("https://api.github.com/bundle", "http://127.0.0.1/").is_err()
        );
        assert!(
            bundle_redirect_target("https://api.github.com/bundle", "https://attacker.example/")
                .is_err()
        );
        assert!(
            bundle_redirect_target(
                "https://api.github.com/bundle",
                "https://api.github.com:8443/bundle",
            )
            .is_err()
        );
        assert_eq!(
            bundle_redirect_target(
                "https://api.github.com/bundle",
                "https://productionresultssa19.blob.core.windows.net/bundle?sig=x",
            )
            .unwrap(),
            "https://productionresultssa19.blob.core.windows.net/bundle?sig=x"
        );
        assert!(
            validate_bundle_url(
                "https://productionresultssa19.blob.core.windows.net/bundle?sig=x",
                GITHUB_BUNDLE_HOSTS,
            )
            .is_ok()
        );
    }

    #[test]
    fn post_delete_and_graphql_use_authenticated_http_requests() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let request = read_request(&mut stream);
            assert!(request.starts_with("POST /comments HTTP/1.1\r\n"));
            assert!(request.contains("Authorization: Bearer test-token\r\n"));
            assert!(request.contains("Accept: application/vnd.github+json\r\n"));
            assert!(request.contains("X-GitHub-Api-Version: 2022-11-28\r\n"));
            assert!(request.contains("\r\n\r\n{\"body\":\"hello\"}"));
            write_response(
                &mut stream,
                "201 Created",
                "Content-Type: application/json\r\n",
                "{\"id\":7}",
            );

            let (mut stream, _) = listener.accept().unwrap();
            let request = read_request(&mut stream);
            assert!(request.starts_with("DELETE /comments/7 HTTP/1.1\r\n"));
            assert!(request.contains("Authorization: Bearer test-token\r\n"));
            assert!(request.contains("Accept: application/vnd.github+json\r\n"));
            assert!(request.contains("X-GitHub-Api-Version: 2022-11-28\r\n"));
            write_response(&mut stream, "204 No Content", "", "");

            let (mut stream, _) = listener.accept().unwrap();
            let request = read_request(&mut stream);
            assert!(request.starts_with("POST /graphql HTTP/1.1\r\n"));
            assert!(request.contains("Authorization: Bearer test-token\r\n"));
            assert!(request.contains("Accept: application/vnd.github+json\r\n"));
            assert!(request.contains("X-GitHub-Api-Version: 2022-11-28\r\n"));
            assert!(request.contains("\"query\":\"query { viewer { login } }\""));
            assert!(request.contains("\"variables\":{\"n\":3}"));
            write_response(
                &mut stream,
                "200 OK",
                "Content-Type: application/json\r\n",
                "{\"data\":{\"viewer\":{\"login\":\"test\"}}}",
            );

            let (mut stream, _) = listener.accept().unwrap();
            let _ = read_request(&mut stream);
            write_response(
                &mut stream,
                "500 Internal Server Error",
                "",
                "secret server failure",
            );

            let (mut stream, _) = listener.accept().unwrap();
            let _ = read_request(&mut stream);
            write_response(&mut stream, "403 Forbidden", "", "secret delete failure");

            let (mut stream, _) = listener.accept().unwrap();
            let _ = read_request(&mut stream);
            write_response(
                &mut stream,
                "200 OK",
                "Content-Type: application/json\r\n",
                "{\"errors\":[{\"message\":\"secret graphql failure\"}]}",
            );
        });
        let agent = ureq::AgentBuilder::new().redirects(0).build();
        let root = format!("http://{address}");
        assert_eq!(
            post_at(
                &agent,
                &root,
                "comments",
                &serde_json::json!({"body":"hello"}),
                "test-token"
            )
            .unwrap(),
            serde_json::json!({"id":7})
        );
        assert!(delete_at(&agent, &root, "comments/7", "test-token").is_ok());
        assert_eq!(
            graphql_at(
                &agent,
                &root,
                "query { viewer { login } }",
                serde_json::json!({"n":3}),
                "test-token"
            )
            .unwrap(),
            serde_json::json!({"viewer":{"login":"test"}})
        );
        let post_error = post_at(
            &agent,
            &root,
            "comments",
            &serde_json::json!({}),
            "test-token",
        )
        .unwrap_err();
        assert_eq!(post_error, "GitHub API returned HTTP 500");
        assert!(!post_error.contains("secret"));
        let delete_error = delete_at(&agent, &root, "comments/7", "test-token").unwrap_err();
        assert_eq!(delete_error, "GitHub API returned HTTP 403");
        assert!(!delete_error.contains("secret"));
        let graphql_error = graphql_at(
            &agent,
            &root,
            "query { viewer { login } }",
            serde_json::json!({}),
            "test-token",
        )
        .unwrap_err();
        assert_eq!(graphql_error, "GitHub GraphQL request failed");
        assert!(!graphql_error.contains("secret"));
        server.join().unwrap();
    }

    #[test]
    fn server_errors_and_malformed_json_are_safe() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            for (status, body) in [
                ("503 Service Unavailable", "token=secret-response-body"),
                ("200 OK", "token=secret-malformed-json{"),
            ] {
                let (mut stream, _) = listener.accept().unwrap();
                let _ = read_request(&mut stream);
                write_response(
                    &mut stream,
                    status,
                    "Content-Type: application/json\r\n",
                    body,
                );
            }
        });
        let agent = ureq::AgentBuilder::new().redirects(0).build();
        let root = format!("http://{address}");
        let failure = get_at(&agent, &root, "failure", "secret-token").unwrap_err();
        assert_eq!(failure, "GitHub API returned HTTP 503");
        assert!(!failure.contains("secret"));
        let malformed = get_at(&agent, &root, "malformed", "secret-token").unwrap_err();
        assert_eq!(malformed, "GitHub API returned invalid JSON");
        assert!(!malformed.contains("secret"));
        server.join().unwrap();
    }

    #[test]
    fn rest_collection_uses_bearer_auth_and_pages_until_short_page() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            for page in [1, 2] {
                let (mut stream, _) = listener.accept().unwrap();
                let mut request = Vec::new();
                let mut byte = [0; 1];
                while !request.ends_with(b"\r\n\r\n") {
                    stream.read_exact(&mut byte).unwrap();
                    request.push(byte[0]);
                }
                let request = String::from_utf8(request).unwrap();
                assert!(request.contains("Authorization: Bearer test-token\r\n"));
                assert!(request.contains(&format!("page={page}")));
                let body = if page == 1 {
                    format!(
                        "[{}]",
                        (1..=100)
                            .map(|n| format!("{{\"number\":{n}}}"))
                            .collect::<Vec<_>>()
                            .join(",")
                    )
                } else {
                    "[{\"number\":101}]".to_owned()
                };
                write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", body.len(), body).unwrap();
            }
        });
        let agent = ureq::AgentBuilder::new().build();
        let values = get_all_at(
            &agent,
            &format!("http://{address}"),
            "repos/o/r/pulls",
            "test-token",
        )
        .unwrap();
        assert_eq!(values.len(), 101);
        server.join().unwrap();
    }
}
