//! GET /v1/providers — list available agent providers.
//!
//! Each provider reports its name, binary, and whether it appears to be
//! installed. Clients (e.g. zzapi) use this to discover which providers
//! can be selected when creating an agent.

use crate::http::reply;
use crate::provider::{DEFAULT_PROVIDER, PROVIDER_NAMES, provider_from_name};
use std::net::TcpStream;
use zz::Json;

pub(crate) fn providers_request(stream: &mut TcpStream) -> Result<(), String> {
    let providers: Vec<Json> = PROVIDER_NAMES
        .iter()
        .map(|name| {
            let provider = provider_from_name(name).expect("known provider name");
            Json::Object(vec![
                ("name".to_owned(), Json::String(provider.name().to_owned())),
                ("bin".to_owned(), Json::String(provider.bin().to_owned())),
                ("available".to_owned(), Json::Bool(provider.is_available())),
                (
                    "default".to_owned(),
                    Json::Bool(provider.name() == DEFAULT_PROVIDER),
                ),
            ])
        })
        .collect();
    reply(
        stream,
        200,
        Json::Object(vec![("providers".to_owned(), Json::Array(providers))]),
    )
}
