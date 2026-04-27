use tonic::transport::Channel;
use url::Url;
use zcash_client_backend::proto::service::{
    compact_tx_streamer_client::CompactTxStreamerClient, Empty, LightdInfo,
};

use crate::{
    error::{ZeckError, ZeckResult},
    models::{LightwalletdProbe, ZeckNetwork},
};

const MAINNET_SAPLING_ACTIVATION: u64 = 419_200;
const TESTNET_SAPLING_ACTIVATION: u64 = 280_000;

pub fn parse_lightwalletd_endpoints(raw: &str) -> Vec<String> {
    let mut endpoints = Vec::new();
    for endpoint in raw
        .split(|ch: char| ch == ',' || ch == ';' || ch.is_whitespace())
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        if !endpoints.iter().any(|existing| existing == endpoint) {
            endpoints.push(endpoint.to_owned());
        }
    }
    endpoints
}

pub fn prioritized_lightwalletd_endpoints(raw: &str, preferred: Option<&str>) -> Vec<String> {
    let mut endpoints = parse_lightwalletd_endpoints(raw);
    if let Some(preferred) = preferred.map(str::trim).filter(|value| !value.is_empty()) {
        if let Some(index) = endpoints.iter().position(|endpoint| endpoint == preferred) {
            let preferred = endpoints.remove(index);
            endpoints.insert(0, preferred);
        } else {
            endpoints.insert(0, preferred.to_owned());
        }
    }
    endpoints
}

pub fn validated_lightwalletd_endpoints(raw: &str) -> ZeckResult<Vec<String>> {
    let endpoints = parse_lightwalletd_endpoints(raw);
    if endpoints.is_empty() {
        return Err(ZeckError::InvalidConfig(
            "at least one lightwalletd endpoint is required".to_owned(),
        ));
    }

    endpoints
        .into_iter()
        .map(|endpoint| validate_lightwalletd_endpoint(&endpoint))
        .collect()
}

pub async fn connect_lightwalletd_endpoints(
    raw: &str,
    preferred: Option<&str>,
) -> ZeckResult<(CompactTxStreamerClient<Channel>, String)> {
    let mut endpoints = validated_lightwalletd_endpoints(raw)?;
    if let Some(preferred) = preferred.map(str::trim).filter(|value| !value.is_empty()) {
        let preferred = validate_lightwalletd_endpoint(preferred)?;
        if let Some(index) = endpoints.iter().position(|endpoint| endpoint == &preferred) {
            let preferred = endpoints.remove(index);
            endpoints.insert(0, preferred);
        } else {
            endpoints.insert(0, preferred);
        }
    }

    let mut errors = Vec::new();
    for endpoint in endpoints {
        match CompactTxStreamerClient::connect(endpoint.clone()).await {
            Ok(client) => return Ok((client, endpoint)),
            Err(err) => errors.push(format!("{endpoint}: {err}")),
        }
    }

    Err(ZeckError::Lightwalletd(format!(
        "failed to connect to any configured lightwalletd endpoint: {}",
        errors.join(" | ")
    )))
}

pub async fn probe_lightwalletd_endpoints(
    raw: &str,
) -> ZeckResult<(CompactTxStreamerClient<Channel>, String, LightdInfo)> {
    let endpoints = validated_lightwalletd_endpoints(raw)?;

    let mut errors = Vec::new();
    for endpoint in endpoints {
        match CompactTxStreamerClient::connect(endpoint.clone()).await {
            Ok(mut client) => match client.get_lightd_info(Empty {}).await {
                Ok(response) => return Ok((client, endpoint, response.into_inner())),
                Err(err) => errors.push(format!("{endpoint}: {err}")),
            },
            Err(err) => errors.push(format!("{endpoint}: {err}")),
        }
    }

    Err(ZeckError::Lightwalletd(format!(
        "all configured lightwalletd endpoints failed probing: {}",
        errors.join(" | ")
    )))
}

pub fn describe_lightwalletd_endpoints(raw: &str) -> String {
    let endpoints = parse_lightwalletd_endpoints(raw);
    match endpoints.as_slice() {
        [] => "no configured endpoints".to_owned(),
        [endpoint] => endpoint.clone(),
        _ => format!("{} endpoints ({})", endpoints.len(), endpoints.join(", ")),
    }
}

pub fn build_probe(endpoint: String, info: &LightdInfo) -> LightwalletdProbe {
    LightwalletdProbe {
        endpoint,
        vendor: Some(info.vendor.clone()),
        chain_name: Some(info.chain_name.clone()),
        latest_block_height: Some(info.block_height),
        sapling_activation_height: Some(info.sapling_activation_height),
    }
}

pub fn validate_lightwalletd_network(network: ZeckNetwork, info: &LightdInfo) -> ZeckResult<()> {
    let chain_name = info.chain_name.trim().to_ascii_lowercase();
    let chain_matches = match network {
        ZeckNetwork::Mainnet => matches!(chain_name.as_str(), "main" | "mainnet"),
        ZeckNetwork::Testnet => matches!(chain_name.as_str(), "test" | "testnet"),
    };
    if !chain_matches {
        return Err(ZeckError::Lightwalletd(format!(
            "server chain '{}' does not match selected {} network",
            info.chain_name,
            network.label()
        )));
    }

    let expected_activation = match network {
        ZeckNetwork::Mainnet => MAINNET_SAPLING_ACTIVATION,
        ZeckNetwork::Testnet => TESTNET_SAPLING_ACTIVATION,
    };
    if info.sapling_activation_height != 0 && info.sapling_activation_height != expected_activation
    {
        return Err(ZeckError::Lightwalletd(format!(
            "server Sapling activation height {} does not match expected {} for {}",
            info.sapling_activation_height,
            expected_activation,
            network.label()
        )));
    }

    Ok(())
}

fn validate_lightwalletd_endpoint(endpoint: &str) -> ZeckResult<String> {
    let url = Url::parse(endpoint).map_err(|err| {
        ZeckError::InvalidConfig(format!("invalid lightwalletd endpoint '{endpoint}': {err}"))
    })?;
    match url.scheme() {
        "https" => {}
        "http" if is_loopback_host(url.host_str()) => {}
        "http" => {
            return Err(ZeckError::InvalidConfig(format!(
                "lightwalletd endpoint '{endpoint}' must use https unless it targets localhost"
            )));
        }
        scheme => {
            return Err(ZeckError::InvalidConfig(format!(
                "lightwalletd endpoint '{endpoint}' uses unsupported scheme '{scheme}'"
            )));
        }
    }

    Ok(endpoint.to_owned())
}

fn is_loopback_host(host: Option<&str>) -> bool {
    host.map(|host| {
        let host = host.to_ascii_lowercase();
        host == "localhost" || host == "127.0.0.1" || host == "::1" || host.ends_with(".localhost")
    })
    .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::{
        describe_lightwalletd_endpoints, parse_lightwalletd_endpoints,
        prioritized_lightwalletd_endpoints, validate_lightwalletd_network,
        validated_lightwalletd_endpoints, TESTNET_SAPLING_ACTIVATION,
    };
    use crate::models::ZeckNetwork;
    use zcash_client_backend::proto::service::LightdInfo;

    #[test]
    fn parse_endpoints_splits_and_deduplicates() {
        let endpoints = parse_lightwalletd_endpoints(
            "https://one.test:9067, https://two.test:9067\nhttps://one.test:9067",
        );

        assert_eq!(
            endpoints,
            vec![
                "https://one.test:9067".to_owned(),
                "https://two.test:9067".to_owned()
            ]
        );
    }

    #[test]
    fn preferred_endpoint_is_prioritized() {
        let endpoints = prioritized_lightwalletd_endpoints(
            "https://one.test:9067,https://two.test:9067",
            Some("https://two.test:9067"),
        );

        assert_eq!(endpoints[0], "https://two.test:9067");
        assert_eq!(endpoints[1], "https://one.test:9067");
    }

    #[test]
    fn describe_empty_returns_sentinel() {
        assert_eq!(describe_lightwalletd_endpoints(""), "no configured endpoints");
    }

    #[test]
    fn describe_single_endpoint_returns_it_verbatim() {
        assert_eq!(
            describe_lightwalletd_endpoints("https://zec.rocks:443"),
            "https://zec.rocks:443"
        );
    }

    #[test]
    fn describe_multiple_endpoints_includes_count() {
        let desc = describe_lightwalletd_endpoints(
            "https://one.test:9067,https://two.test:9067,https://three.test:9067",
        );
        assert!(desc.starts_with("3 endpoints"), "got: {desc}");
        assert!(desc.contains("https://one.test:9067"));
    }

    #[test]
    fn preferred_endpoint_not_in_list_is_prepended() {
        let endpoints = prioritized_lightwalletd_endpoints(
            "https://one.test:9067",
            Some("https://preferred.test:9067"),
        );
        assert_eq!(endpoints[0], "https://preferred.test:9067");
        assert_eq!(endpoints[1], "https://one.test:9067");
    }

    #[test]
    fn empty_preferred_is_ignored() {
        let endpoints = prioritized_lightwalletd_endpoints(
            "https://one.test:9067,https://two.test:9067",
            Some("  "),
        );
        assert_eq!(endpoints[0], "https://one.test:9067");
        assert_eq!(endpoints.len(), 2);
    }

    #[test]
    fn endpoint_validation_rejects_remote_plaintext_http() {
        let err = validated_lightwalletd_endpoints("http://wallet.example:9067")
            .expect_err("remote http should be rejected");

        assert!(err.to_string().contains("must use https"));
    }

    #[test]
    fn endpoint_validation_allows_loopback_http_for_local_testing() {
        let endpoints = validated_lightwalletd_endpoints("http://127.0.0.1:9067")
            .expect("loopback http should be allowed");

        assert_eq!(endpoints, vec!["http://127.0.0.1:9067".to_owned()]);
    }

    #[test]
    fn network_validation_rejects_wrong_chain() {
        let info = LightdInfo {
            chain_name: "test".to_owned(),
            sapling_activation_height: TESTNET_SAPLING_ACTIVATION,
            ..LightdInfo::default()
        };
        let err = validate_lightwalletd_network(ZeckNetwork::Mainnet, &info)
            .expect_err("wrong chain should be rejected");

        assert!(err.to_string().contains("does not match selected mainnet"));
    }
}
