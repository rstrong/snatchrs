//! Shared transport defaults for discovery, metadata, and artifact clients.
use std::time::Duration;

use reqwest::blocking::{Client, ClientBuilder};

pub(crate) fn client_builder() -> ClientBuilder {
    // Keep reqwest's automatic environment-proxy discovery enabled. Installing
    // an explicit Proxy or calling no_proxy() here would override its handling
    // of HTTP_PROXY, HTTPS_PROXY, ALL_PROXY, NO_PROXY and lowercase equivalents.
    Client::builder()
        .user_agent(concat!("snatchrs/", env!("CARGO_PKG_VERSION")))
        .connect_timeout(Duration::from_secs(15))
}
