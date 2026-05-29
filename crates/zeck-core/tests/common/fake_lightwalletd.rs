//! `FakeLightwalletd` — an in-process gRPC server speaking the lightwalletd
//! wire protocol, used by the C2 integration tests.
//!
//! This is the **skeleton + proxy-mode** scaffolding: it boots a tonic server
//! on a loopback ephemeral port, answers `GetLightdInfo` from a builder-
//! supplied config, and (when given an upstream URL) forwards every other RPC
//! Argos uses to a real lightwalletd instance. The fault-injection knobs
//! (`latency`, `close_stream_after_blocks`, `inject_hostile_block_at_height`)
//! are wired into the builder but not yet implemented — the follow-up PR will
//! flesh them out to unblock R-N8 (GoAway mid-stream) and R-N9 (hostile
//! compact block).
//!
//! Why proxy mode: it lets the bad-network tests reuse the real regtest
//! harness's compact-block stream for the happy-path prefix, then introduce
//! a fault at a chosen point. Synthesising a full chain in-process would be
//! an order-of-magnitude larger project.
//!
//! Gated entirely behind `cfg(feature = "argos-network")` — production builds
//! never see this file.

#![cfg(feature = "argos-network")]
#![allow(dead_code)] // Fields and helpers exist for the follow-up fault-injection PR.

use std::net::SocketAddr;
use std::time::Duration;

use tokio::net::TcpListener;
use tokio::sync::mpsc;
use tokio_stream::wrappers::{ReceiverStream, TcpListenerStream};
use tonic::transport::{Channel, Server};
use tonic::{Request, Response, Status};

// Re-export the generated proto code under a stable in-crate path. The
// `cash.z.wallet.sdk.rpc` package name produces an output file whose name
// contains dots; `include!` swallows that and exposes the contents inside
// this `pb` module.
#[rustfmt::skip]
#[allow(clippy::all, clippy::pedantic, missing_docs)]
mod pb {
    include!(concat!(env!("OUT_DIR"), "/cash.z.wallet.sdk.rpc.rs"));
}

use pb::compact_tx_streamer_server::{CompactTxStreamer, CompactTxStreamerServer};
use pb::{
    BlockId, BlockRange, CompactBlock, Empty, GetAddressUtxosArg, GetAddressUtxosReplyList,
    LightdInfo, RawTransaction, SendResponse, TransparentAddressBlockFilter, TreeState, TxFilter,
};

// Argos's runtime client. The fixture uses the same generated client
// `zcash_client_backend::proto` ships to forward unknown RPCs upstream, so
// the wire format is guaranteed to match what Argos itself sends.
use zcash_client_backend::proto::service::compact_tx_streamer_client::CompactTxStreamerClient
    as UpstreamClient;

/// A running fake lightwalletd instance.
///
/// Listens on `127.0.0.1:NNNNN` (random ephemeral port). The server task is
/// owned by the `JoinHandle` in `_handle`; dropping the `FakeLightwalletd`
/// aborts the server task and frees the port.
pub struct FakeLightwalletd {
    /// Loopback URL the fixture is listening on, e.g. `http://127.0.0.1:NNNNN`.
    /// Pass this directly to `RecoveryService::start_scan`'s
    /// `lightwalletd_url`. Argos's `validate_lightwalletd_endpoint` accepts
    /// `http://127.0.0.1:...` because the host is loopback.
    pub url: String,
    /// JoinHandle for the tonic server task; dropping shuts the server down.
    _handle: tokio::task::JoinHandle<()>,
}

impl FakeLightwalletd {
    /// Begin building a new fixture. See [`FakeLightwalletdBuilder`].
    pub fn builder() -> FakeLightwalletdBuilder {
        FakeLightwalletdBuilder::default()
    }
}

/// Builder for [`FakeLightwalletd`].
///
/// Chain identity defaults are picked to satisfy `validate_lightwalletd_network`
/// when the operator selected `ZeckNetwork::Testnet` under the `argos-network`
/// feature: `chain_name = "regtest"`, `sapling_activation_height = 1` (the
/// Sapling-height check is skipped for the regtest chain name).
#[derive(Clone, Debug)]
pub struct FakeLightwalletdBuilder {
    /// `chain_name` returned by `GetLightdInfo`.
    chain_name: String,
    /// `sapling_activation_height` returned by `GetLightdInfo`.
    sapling_activation_height: u64,
    /// `block_height` returned by `GetLightdInfo` when no upstream is set.
    /// In proxy mode the upstream's value is forwarded instead.
    block_height: u64,
    /// If set, forward every RPC other than `GetLightdInfo` to this real
    /// lightwalletd. Must be a URL `tonic::transport::Channel::connect` accepts
    /// (e.g. `http://127.0.0.1:9067` for the regtest harness).
    upstream_url: Option<String>,
    // ─── Fault-injection knobs (TODO follow-up PR) ──────────────────────
    /// Inject a fixed per-RPC latency before responding.
    ///
    /// TODO(R-N8/R-N9): implement.
    latency: Option<Duration>,
    /// Close the `GetBlockRange` stream cleanly after this many compact
    /// blocks have been emitted, causing the client to observe a connection
    /// drop / GoAway. Unblocks R-N8.
    ///
    /// TODO(R-N8/R-N9): implement.
    close_stream_after_blocks: Option<u64>,
    /// When the upstream returns the compact block at this height, substitute
    /// a deliberately malformed block to exercise the client's parser/anchor
    /// rejection path. Unblocks R-N9.
    ///
    /// TODO(R-N8/R-N9): implement.
    inject_hostile_block_at_height: Option<u64>,
}

impl Default for FakeLightwalletdBuilder {
    fn default() -> Self {
        Self {
            chain_name: "regtest".to_owned(),
            sapling_activation_height: 1,
            block_height: 0,
            upstream_url: None,
            latency: None,
            close_stream_after_blocks: None,
            inject_hostile_block_at_height: None,
        }
    }
}

impl FakeLightwalletdBuilder {
    /// Set the `chain_name` returned by `GetLightdInfo`. Defaults to
    /// `"regtest"`, which is what the `argos-network`-gated branch of
    /// `validate_lightwalletd_network` expects.
    pub fn chain_name(mut self, name: impl Into<String>) -> Self {
        self.chain_name = name.into();
        self
    }

    /// Override the Sapling activation height reported in `GetLightdInfo`.
    /// Defaults to 1 (regtest).
    pub fn sapling_activation_height(mut self, height: u64) -> Self {
        self.sapling_activation_height = height;
        self
    }

    /// Override the latest block height reported in `GetLightdInfo`. Ignored
    /// in proxy mode (the upstream's value is forwarded).
    pub fn block_height(mut self, height: u64) -> Self {
        self.block_height = height;
        self
    }

    /// Forward unknown RPCs to a real upstream lightwalletd. Required for
    /// every RPC except `GetLightdInfo` in this skeleton PR; the follow-up
    /// PR will add synthetic responders for the fault-injection cases that
    /// don't need a real backend.
    pub fn upstream(mut self, url: impl Into<String>) -> Self {
        self.upstream_url = Some(url.into());
        self
    }

    /// Inject a fixed latency before each RPC response.
    ///
    /// TODO(R-N8/R-N9): implement.
    pub fn latency(mut self, latency: Duration) -> Self {
        self.latency = Some(latency);
        self
    }

    /// Close the `GetBlockRange` stream after this many compact blocks,
    /// simulating an HTTP/2 GoAway mid-stream (R-N8).
    ///
    /// TODO(R-N8/R-N9): implement.
    pub fn close_stream_after_blocks(mut self, count: u64) -> Self {
        self.close_stream_after_blocks = Some(count);
        self
    }

    /// Inject a hostile compact block at the given height (R-N9).
    ///
    /// TODO(R-N8/R-N9): implement.
    pub fn inject_hostile_block_at_height(mut self, height: u64) -> Self {
        self.inject_hostile_block_at_height = Some(height);
        self
    }

    /// Bind a TCP listener on `127.0.0.1:0`, hand it to a tonic server, and
    /// return a `FakeLightwalletd` whose `url` points at the assigned port.
    pub async fn build(self) -> std::io::Result<FakeLightwalletd> {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let local_addr: SocketAddr = listener.local_addr()?;
        let url = format!("http://{local_addr}");

        let service = FakeService::new(self).await;
        let server = CompactTxStreamerServer::new(service);

        let incoming = TcpListenerStream::new(listener);
        let handle = tokio::spawn(async move {
            let _ = Server::builder()
                .add_service(server)
                .serve_with_incoming(incoming)
                .await;
        });

        Ok(FakeLightwalletd {
            url,
            _handle: handle,
        })
    }
}

/// Inner gRPC service implementing the (pruned) `CompactTxStreamer` trait.
struct FakeService {
    chain_name: String,
    sapling_activation_height: u64,
    block_height: u64,
    /// Lazily-connected upstream client. `None` means no proxy mode; every
    /// RPC other than `GetLightdInfo` returns `unimplemented` until the
    /// follow-up PR adds synthetic responders.
    upstream: Option<UpstreamClient<Channel>>,
    // Fault-injection state (placeholder — see TODO in builder).
    _latency: Option<Duration>,
    _close_stream_after_blocks: Option<u64>,
    _inject_hostile_block_at_height: Option<u64>,
}

impl FakeService {
    async fn new(builder: FakeLightwalletdBuilder) -> Self {
        let upstream = match builder.upstream_url.as_deref() {
            Some(url) => {
                // We deliberately propagate connection errors through a
                // `panic` here rather than surfacing them: this only runs in
                // the test fixture, and a missing upstream is a test-setup
                // bug the developer wants to see loudly.
                let client = UpstreamClient::connect(url.to_owned())
                    .await
                    .unwrap_or_else(|err| {
                        panic!("FakeLightwalletd could not connect to upstream {url}: {err}")
                    });
                Some(client)
            }
            None => None,
        };

        Self {
            chain_name: builder.chain_name,
            sapling_activation_height: builder.sapling_activation_height,
            block_height: builder.block_height,
            upstream,
            _latency: builder.latency,
            _close_stream_after_blocks: builder.close_stream_after_blocks,
            _inject_hostile_block_at_height: builder.inject_hostile_block_at_height,
        }
    }

    /// Return a fresh clone of the upstream client, or an `unavailable` Status
    /// when proxy mode is disabled. The clone is cheap — tonic channels share
    /// the underlying HTTP/2 connection.
    fn upstream(&self) -> Result<UpstreamClient<Channel>, Status> {
        self.upstream.clone().ok_or_else(|| {
            Status::unimplemented(
                "FakeLightwalletd: this RPC requires an upstream lightwalletd; \
                 call .upstream(url) on the builder",
            )
        })
    }
}

/// Translate between the *locally-generated* proto types (under `pb::`) and
/// the *upstream client's* proto types (under `zcash_client_backend::proto::*`).
/// The wire format is identical, so we go through `prost`'s
/// `Message::encode`/`decode`. This avoids leaking the upstream's type names
/// into the rest of the fixture and lets the proxy treat upstream/downstream
/// independently.
fn reencode<A, B>(from: &A) -> B
where
    A: prost::Message,
    B: prost::Message + Default,
{
    let mut buf = Vec::with_capacity(from.encoded_len());
    from.encode(&mut buf)
        .expect("prost::Message::encode into Vec cannot fail");
    B::decode(&buf[..]).expect("prost wire format is identical between locally- and upstream-generated types")
}

#[tonic::async_trait]
impl CompactTxStreamer for FakeService {
    async fn get_lightd_info(
        &self,
        _request: Request<Empty>,
    ) -> Result<Response<LightdInfo>, Status> {
        // In proxy mode we still synthesise the response so the fixture can
        // claim its own chain identity (the upstream may legitimately be a
        // testnet or mainnet server; the fixture should still look like a
        // regtest to Argos under argos-network).
        let block_height = if let Some(mut upstream) = self.upstream.clone() {
            // Best-effort: ask the upstream for its tip height so the proxy
            // mode reports a realistic value, but fall back to the configured
            // value if the upstream call fails.
            match upstream
                .get_lightd_info(zcash_client_backend::proto::service::Empty {})
                .await
            {
                Ok(info) => info.into_inner().block_height,
                Err(_) => self.block_height,
            }
        } else {
            self.block_height
        };

        Ok(Response::new(LightdInfo {
            vendor: "argos-fake-lightwalletd".to_owned(),
            chain_name: self.chain_name.clone(),
            sapling_activation_height: self.sapling_activation_height,
            block_height,
            ..LightdInfo::default()
        }))
    }

    async fn get_block(
        &self,
        request: Request<BlockId>,
    ) -> Result<Response<CompactBlock>, Status> {
        let mut upstream = self.upstream()?;
        let req = reencode::<_, zcash_client_backend::proto::service::BlockId>(
            request.get_ref(),
        );
        let resp = upstream.get_block(req).await?.into_inner();
        Ok(Response::new(reencode::<_, CompactBlock>(&resp)))
    }

    type GetBlockRangeStream = ReceiverStream<Result<CompactBlock, Status>>;

    async fn get_block_range(
        &self,
        request: Request<BlockRange>,
    ) -> Result<Response<Self::GetBlockRangeStream>, Status> {
        // TODO(R-N8/R-N9): wrap this stream with the fault-injection logic
        //   (close_stream_after_blocks for R-N8, hostile-block substitution
        //   for R-N9).
        let mut upstream = self.upstream()?;
        let req = reencode::<_, zcash_client_backend::proto::service::BlockRange>(
            request.get_ref(),
        );
        let mut upstream_stream = upstream.get_block_range(req).await?.into_inner();

        let (tx, rx) = mpsc::channel::<Result<CompactBlock, Status>>(16);
        tokio::spawn(async move {
            while let Some(item) = upstream_stream.message().await.transpose() {
                let mapped = match item {
                    Ok(block) => Ok(reencode::<_, CompactBlock>(&block)),
                    Err(status) => Err(Status::new(status.code(), status.message())),
                };
                if tx.send(mapped).await.is_err() {
                    break;
                }
            }
        });
        Ok(Response::new(ReceiverStream::new(rx)))
    }

    async fn get_transaction(
        &self,
        request: Request<TxFilter>,
    ) -> Result<Response<RawTransaction>, Status> {
        let mut upstream = self.upstream()?;
        let req = reencode::<_, zcash_client_backend::proto::service::TxFilter>(
            request.get_ref(),
        );
        let resp = upstream.get_transaction(req).await?.into_inner();
        Ok(Response::new(reencode::<_, RawTransaction>(&resp)))
    }

    async fn send_transaction(
        &self,
        request: Request<RawTransaction>,
    ) -> Result<Response<SendResponse>, Status> {
        let mut upstream = self.upstream()?;
        let req = reencode::<_, zcash_client_backend::proto::service::RawTransaction>(
            request.get_ref(),
        );
        let resp = upstream.send_transaction(req).await?.into_inner();
        Ok(Response::new(reencode::<_, SendResponse>(&resp)))
    }

    type GetTaddressTxidsStream = ReceiverStream<Result<RawTransaction, Status>>;

    async fn get_taddress_txids(
        &self,
        request: Request<TransparentAddressBlockFilter>,
    ) -> Result<Response<Self::GetTaddressTxidsStream>, Status> {
        let mut upstream = self.upstream()?;
        let req = reencode::<
            _,
            zcash_client_backend::proto::service::TransparentAddressBlockFilter,
        >(request.get_ref());
        let mut upstream_stream = upstream.get_taddress_txids(req).await?.into_inner();

        let (tx, rx) = mpsc::channel::<Result<RawTransaction, Status>>(16);
        tokio::spawn(async move {
            while let Some(item) = upstream_stream.message().await.transpose() {
                let mapped = match item {
                    Ok(raw) => Ok(reencode::<_, RawTransaction>(&raw)),
                    Err(status) => Err(Status::new(status.code(), status.message())),
                };
                if tx.send(mapped).await.is_err() {
                    break;
                }
            }
        });
        Ok(Response::new(ReceiverStream::new(rx)))
    }

    async fn get_tree_state(
        &self,
        request: Request<BlockId>,
    ) -> Result<Response<TreeState>, Status> {
        let mut upstream = self.upstream()?;
        let req = reencode::<_, zcash_client_backend::proto::service::BlockId>(
            request.get_ref(),
        );
        let resp = upstream.get_tree_state(req).await?.into_inner();
        Ok(Response::new(reencode::<_, TreeState>(&resp)))
    }

    async fn get_address_utxos(
        &self,
        request: Request<GetAddressUtxosArg>,
    ) -> Result<Response<GetAddressUtxosReplyList>, Status> {
        let mut upstream = self.upstream()?;
        let req = reencode::<
            _,
            zcash_client_backend::proto::service::GetAddressUtxosArg,
        >(request.get_ref());
        let resp = upstream.get_address_utxos(req).await?.into_inner();
        Ok(Response::new(reencode::<_, GetAddressUtxosReplyList>(&resp)))
    }
}
