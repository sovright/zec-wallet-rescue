use tonic::transport::Channel;
use zcash_client_backend::proto::service::{
    compact_tx_streamer_client::CompactTxStreamerClient, Empty, LightdInfo,
};

use crate::{
    error::{ZeckError, ZeckResult},
    models::LightwalletdProbe,
};

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

pub async fn connect_lightwalletd_endpoints(
    raw: &str,
    preferred: Option<&str>,
) -> ZeckResult<(CompactTxStreamerClient<Channel>, String)> {
    let endpoints = prioritized_lightwalletd_endpoints(raw, preferred);
    if endpoints.is_empty() {
        return Err(ZeckError::InvalidConfig(
            "at least one lightwalletd endpoint is required".to_owned(),
        ));
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
    let endpoints = parse_lightwalletd_endpoints(raw);
    if endpoints.is_empty() {
        return Err(ZeckError::InvalidConfig(
            "at least one lightwalletd endpoint is required".to_owned(),
        ));
    }

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

#[cfg(test)]
mod tests {
    use super::{parse_lightwalletd_endpoints, prioritized_lightwalletd_endpoints};

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
}
