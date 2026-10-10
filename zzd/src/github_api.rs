//! Small authenticated GitHub API client shared by PR, comment, and review flows.

use serde_json::Value;
use std::io::Read;
use std::path::PathBuf;

const API_ROOT: &str = "https://api.github.com";

fn github_token() -> Result<String, String> {
    for name in ["GH_TOKEN", "GITHUB_TOKEN"] {
        if let Some(token) = std::env::var(name)
            .ok()
            .filter(|token| !token.trim().is_empty())
        {
            return Ok(token);
        }
    }

    // Resolve the same host configuration used by the GitHub CLI. Its secure
    // credential-store entry is keyed by the active username (and it keeps an
    // unkeyed active-account entry for older configurations).
    let config = std::env::var_os("GH_CONFIG_DIR")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("XDG_CONFIG_HOME").map(|path| PathBuf::from(path).join("gh")))
        .or_else(|| std::env::var_os("HOME").map(|path| PathBuf::from(path).join(".config/gh")))
        .ok_or_else(|| {
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
        && let Ok(entry) = keyring::Entry::new("gh:github.com", user)
        && let Ok(token) = entry.get_password()
        && !token.trim().is_empty()
    {
        return Ok(token);
    }
    if let Ok(entry) = keyring::Entry::new("gh:github.com", "")
        && let Ok(token) = entry.get_password()
        && !token.trim().is_empty()
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
    let agent = ureq::AgentBuilder::new()
        .user_agent(concat!("zigzag/", env!("CARGO_PKG_VERSION")))
        .build();
    Ok((agent, token))
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
        Err(ureq::Error::Status(code, response)) => {
            let detail = response.into_string().unwrap_or_default();
            return Err(format!(
                "GitHub API returned HTTP {code}: {}",
                detail.chars().take(500).collect::<String>()
            ));
        }
        Err(ureq::Error::Transport(error)) => {
            return Err(format!("GitHub API request failed: {error}"));
        }
    };
    response
        .into_json()
        .map_err(|error| format!("GitHub API returned invalid JSON: {error}"))
}

pub(crate) fn get(endpoint: &str) -> Result<Value, String> {
    let (agent, token) = agent()?;
    response_json(headers(agent.get(&format!("{API_ROOT}/{endpoint}")), &token).call())
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
    for page in 1..=1000 {
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
    response_json(
        headers(agent.post(&format!("{API_ROOT}/{endpoint}")), &token).send_json(body.clone()),
    )
}

pub(crate) fn delete(endpoint: &str) -> Result<(), String> {
    let (agent, token) = agent()?;
    match headers(agent.delete(&format!("{API_ROOT}/{endpoint}")), &token).call() {
        Ok(_) => Ok(()),
        Err(ureq::Error::Status(204, _)) => Ok(()),
        Err(ureq::Error::Status(code, response)) => Err(format!(
            "GitHub API returned HTTP {code}: {}",
            response
                .into_string()
                .unwrap_or_default()
                .chars()
                .take(500)
                .collect::<String>()
        )),
        Err(ureq::Error::Transport(error)) => Err(format!("GitHub API request failed: {error}")),
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
    let response = response_json(
        headers(
            agent.get(&format!(
                "{api_root}/repos/{repository}/attestations/sha256:{digest}?per_page=100"
            )),
            token,
        )
        .call(),
    )?;
    let attestations = response
        .get("attestations")
        .and_then(Value::as_array)
        .ok_or_else(|| "GitHub did not return artifact attestations".to_owned())?;
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
            let response = agent.get(url).call().map_err(|error| {
                format!("could not download GitHub attestation bundle: {error}")
            })?;
            let mut bytes = Vec::new();
            response
                .into_reader()
                .take(32 * 1024 * 1024)
                .read_to_end(&mut bytes)
                .map_err(|error| format!("could not read GitHub attestation bundle: {error}"))?;
            decode_snappy_frame(&bytes)
        })
        .collect()
}

fn decode_snappy_frame(bytes: &[u8]) -> Result<Vec<u8>, String> {
    const IDENTIFIER: &[u8] = b"\xff\x06\x00\x00sNaPpY";
    if bytes.first().is_some_and(|byte| *byte == b'{') {
        return Ok(bytes.to_vec());
    }
    if !bytes.starts_with(IDENTIFIER) {
        return Err("GitHub attestation bundle has an unsupported encoding".to_owned());
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
    let response = post(
        "graphql",
        &serde_json::json!({"query": query, "variables": variables}),
    )?;
    if let Some(errors) = response.get("errors") {
        return Err(format!("GitHub GraphQL request failed: {errors}"));
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

    #[test]
    fn snappy_bundle_decoder_reads_uncompressed_and_literal_chunks() {
        let identifier = b"\xff\x06\x00\x00sNaPpY";
        let block = [0x02, 0x04, b'o', b'k'];
        let mut frame = identifier.to_vec();
        frame.extend([0x00, (block.len() + 4) as u8, 0, 0]);
        frame.extend(masked_snappy_crc(b"ok").to_le_bytes());
        frame.extend(block);
        assert_eq!(decode_snappy_frame(&frame).unwrap(), b"ok");

        let mut uncompressed = identifier.to_vec();
        uncompressed.extend([0x01, 0x06, 0, 0]);
        uncompressed.extend(masked_snappy_crc(b"ok").to_le_bytes());
        uncompressed.extend(b"ok");
        assert_eq!(decode_snappy_frame(&uncompressed).unwrap(), b"ok");
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
    fn attestations_are_fetched_through_api_and_presigned_url_without_token() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            for request_index in 0..2 {
                let (mut stream, _) = listener.accept().unwrap();
                let mut request = Vec::new();
                let mut byte = [0; 1];
                while !request.ends_with(b"\r\n\r\n") {
                    stream.read_exact(&mut byte).unwrap();
                    request.push(byte[0]);
                }
                let request = String::from_utf8(request).unwrap();
                if request_index == 0 {
                    assert!(request.contains("Authorization: Bearer test-token\r\n"));
                    assert!(request.contains("/repos/o/r/attestations/sha256:abc"));
                    let body = format!(
                        "{{\"attestations\":[{{\"bundle_url\":\"http://{address}/bundle\"}}]}}"
                    );
                    write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", body.len(), body).unwrap();
                } else {
                    assert!(!request.to_ascii_lowercase().contains("authorization:"));
                    let body = "{\"bundle\":true}";
                    write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", body.len(), body).unwrap();
                }
            }
        });
        let agent = ureq::AgentBuilder::new().build();
        let bundles = attestation_bundles_at(
            &agent,
            &format!("http://{address}"),
            "o/r",
            "abc",
            "test-token",
        )
        .unwrap();
        assert_eq!(bundles, [b"{\"bundle\":true}".to_vec()]);
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
