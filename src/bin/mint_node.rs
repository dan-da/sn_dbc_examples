// Copyright 2022 MaidSafe.net limited.
//
// This SAFE Network Software is licensed to you under the MIT license <LICENSE-MIT
// http://opensource.org/licenses/MIT> or the Modified BSD license <LICENSE-BSD
// https://opensource.org/licenses/BSD-3-Clause>, at your option. This file may not be copied,
// modified, or distributed except according to those terms. Please review the Licences for the
// specific language governing permissions and limitations relating to use of the SAFE Network
// Software.

//! Safe Network DBC Mint Node example.
#![allow(clippy::from_iter_instead_of_collect)]

use log::{debug, info, trace};
use miette::{IntoDiagnostic, Result};
use serde::{Deserialize, Serialize};

// use blst_ringct::ringct::{RingCtMaterial, RingCtTransaction};
// use blst_ringct::RevealedCommitment;
// use blsttc::poly::Poly;
// use blsttc::serde_impl::SerdeSecret;
// use blsttc::{PublicKey, PublicKeySet, SecretKey, SecretKeySet, SecretKeyShare};
// use rand::seq::IteratorRandom;
// use rand::Rng;
// use rand8::SeedableRng;
// use serde::{Deserialize, Serialize};
use sn_dbc::{
    MintNode,
    SimpleKeyManager,
    SimpleSigner,
    // Amount, Dbc, DbcBuilder, GenesisBuilderMock, MintNode, Output, OutputOwnerMap, Owner,
    // OwnerOnce, ReissueRequest, ReissueRequestBuilder, SimpleKeyManager, SpentBookNodeMock,
    // TransactionBuilder,
};
// use std::collections::{BTreeMap, HashMap};
// use std::iter::FromIterator;

use xor_name::XorName;

use qp2p::{self, Config, Endpoint, IncomingConnections};
use structopt::StructOpt;

use bls_dkg::KeyGen;
use rand_core::RngCore;
use std::collections::{BTreeMap, BTreeSet};
use std::net::{Ipv4Addr, SocketAddr};

/// Configuration for the program
#[derive(StructOpt)]
pub struct MintNodeConfig {
    /// Peer addresses (other MintNodes)
    peers: Vec<SocketAddr>,

    #[structopt(flatten)]
    mint_qp2p_opts: Config,
    // we would like to do the following, but not (yet?) supported.
    // filed this: https://github.com/clap-rs/clap/issues/3443
    // #[structopt(flatten, prefix="wallet")]
    // wallet_qp2p_opts: Config,

    // #[structopt(flatten, prefix="mint")]
    // mint_qp2p_opts: Config,
}

struct ServerEndpoint {
    endpoint: Endpoint,
    incoming_connections: IncomingConnections,
}

struct MintNodeServer {
    xor_name: XorName,

    config: MintNodeConfig,

    peers: BTreeMap<XorName, SocketAddr>,

    mint_node: Option<MintNode<SimpleKeyManager>>,

    /// for communicating with other mintnodes
    mint_endpoint: ServerEndpoint,

    /// for communicating with wallet users
    wallet_endpoint: ServerEndpoint,

    keygen: Option<bls_dkg::KeyGen>,
}

#[derive(Debug, Serialize, Deserialize)]
enum MintNetworkMsg {
    Peer(XorName, SocketAddr),
    Dkg(bls_dkg::message::Message),
}

#[derive(Debug, Serialize, Deserialize)]
enum WalletNetworkMsg {
    Rpc(String),
}

#[tokio::main]
async fn main() -> Result<()> {
    let result = do_main().await;
    match result {
        Ok(_) => Ok(()),
        Err(e) => {
            println!("{}", e);
            Err(e)
        }
    }
}

async fn do_main() -> Result<()> {
    env_logger::Builder::from_env(
        env_logger::Env::default().default_filter_or("qp2p=warn,quinn=warn"),
    )
    //    .format(|buf, record| writeln!(buf, "{}\n", record.args()))
    .init();

    // let mut rng = rand::thread_rng();
    let config = MintNodeConfig::from_args();

    let (endpoint, incoming_connections, _contact) = Endpoint::new_peer(
        SocketAddr::from((Ipv4Addr::LOCALHOST, 0)),
        &[],
        config.mint_qp2p_opts.clone(),
    )
    .await
    .into_diagnostic()?;
    let mint_endpoint = ServerEndpoint {
        endpoint,
        incoming_connections,
    };

    let mut wallet_qp2p_opts = config.mint_qp2p_opts.clone();
    wallet_qp2p_opts.external_port = config.mint_qp2p_opts.external_port.map(|p| p + 1);

    let (endpoint, incoming_connections, _contact) = Endpoint::new_peer(
        SocketAddr::from((Ipv4Addr::LOCALHOST, 0)),
        &[],
        wallet_qp2p_opts,
    )
    .await
    .into_diagnostic()?;
    let wallet_endpoint = ServerEndpoint {
        endpoint,
        incoming_connections,
    };

    let my_xor_name = XorName::random();

    println!(
        "Mint [{}] listening for messages at: {}",
        my_xor_name,
        mint_endpoint.endpoint.public_addr()
    );

    let mut my_node = MintNodeServer {
        config,
        xor_name: my_xor_name,
        peers: BTreeMap::from_iter([(my_xor_name, mint_endpoint.endpoint.public_addr())]),
        mint_node: None,
        mint_endpoint,
        wallet_endpoint,
        keygen: None,
    };

    my_node.run().await?;

    Ok(())
}

impl MintNodeServer {
    async fn run(&mut self) -> Result<()> {
        for peer in self.config.peers.clone().iter() {
            let msg =
                MintNetworkMsg::Peer(self.xor_name, self.mint_endpoint.endpoint.public_addr());
            self.send_mint_network_msg(&msg, peer).await?;
        }

        self.listen_for_mint_network_msgs().await
        // self.listen_for_wallet_network_msgs().await;

        // futures::try_join!(mint_future, wallet_future).map(|_| ())
        // tokio::join!(mint_future, wallet_future);
        // Ok(())
    }

    async fn listen_for_mint_network_msgs(&mut self) -> Result<()> {
        let local_addr = self.mint_endpoint.endpoint.local_addr();
        let external_addr = self.mint_endpoint.endpoint.public_addr();
        info!(
            "[P2P] listening on local  {:?}, external: {:?}",
            local_addr, external_addr
        );

        while let Some((connection, mut incoming_messages)) =
            self.mint_endpoint.incoming_connections.next().await
        {
            let socket_addr = connection.remote_address();

            while let Some(bytes) = incoming_messages.next().await.into_diagnostic()? {
                // async version
                let net_msg: MintNetworkMsg = bincode::deserialize(&bytes).into_diagnostic()?;

                debug!("[P2P] received from {:?} --> {:?}", socket_addr, net_msg);
                let mut rng = rand::thread_rng();
                // let mut rng: rand::rngs::OsRng = Default::default();

                match net_msg {
                    MintNetworkMsg::Peer(actor, addr) => self.handle_peer_msg(actor, addr).await?,
                    MintNetworkMsg::Dkg(msg) => self.handle_dkg_message(msg, &mut rng).await?,
                }
            }
        }

        info!("[P2P] Finished listening for incoming messages");
        Ok(())
    }

    async fn listen_for_wallet_network_msgs(&mut self) -> Result<()> {
        let local_addr = self.wallet_endpoint.endpoint.local_addr();
        let external_addr = self.wallet_endpoint.endpoint.public_addr();
        info!(
            "[Wallet] listening on local  {:?}, external: {:?}",
            local_addr, external_addr
        );

        while let Some((connection, mut incoming_messages)) =
            self.wallet_endpoint.incoming_connections.next().await
        {
            let socket_addr = connection.remote_address();

            while let Some(bytes) = incoming_messages.next().await.into_diagnostic()? {
                // async version
                let net_msg: WalletNetworkMsg = bincode::deserialize(&bytes).into_diagnostic()?;

                debug!("[P2P] received from {:?} --> {:?}", socket_addr, net_msg);

                match net_msg {
                    WalletNetworkMsg::Rpc(json) => self.handle_json_request(json).await?,
                }
            }
        }

        info!("[Wallet] Finished listening for incoming messages");
        Ok(())
    }

    async fn handle_json_request(&self, _json: String) -> Result<()> {
        Ok(())
    }

    async fn send_mint_network_msg(
        &self,
        msg: &MintNetworkMsg,
        dest_addr: &SocketAddr,
    ) -> Result<()> {
        // if delivering to self, use local addr rather than external to avoid
        // potential hairpinning problems.
        let addr = if *dest_addr == self.mint_endpoint.endpoint.public_addr() {
            self.mint_endpoint.endpoint.local_addr()
        } else {
            *dest_addr
        };

        debug!("[P2P] Sending message to {:?} --> {:?}", addr, msg);

        // fixme: unwrap
        let msg = bincode::serialize(msg).unwrap();

        let (connection, _) = self
            .mint_endpoint
            .endpoint
            .connect_to(&addr)
            .await
            .into_diagnostic()?;
        // {
        //     error!("[P2P] Failed to connect to {}. {:?}", addr, e);
        //     return;
        // }

        // debug!(
        //     "[P2P] Sending message to {:?} --> {:?}",
        //     addr, msg
        // );

        connection.send(msg.into()).await.into_diagnostic()
        // {
        //     Ok(()) => trace!("[P2P] Sent network msg successfully."),
        //     Err(e) => error!("[P2P] Failed to send network msg: {:?}", e),
        // }
    }

    async fn handle_peer_msg(&mut self, actor: XorName, addr: SocketAddr) -> Result<()> {
        if self.peers.contains_key(&actor) {
            trace!(
                "We already know about peer [{:?}]@{:?}. ignoring.",
                actor,
                addr
            )
        } else {
            // Here we send our peer list back to the new peer.
            for (peer_actor, peer_addr) in self.peers.clone().into_iter() {
                self.send_mint_network_msg(&MintNetworkMsg::Peer(peer_actor, peer_addr), &addr)
                    .await?;
            }
            self.peers.insert(actor, addr);

            trace!("Added peer [{:?}]@{:?}", actor, addr);

            if self.peers.len() == 3 {
                println!("initiating dkg with {} nodes", self.peers.len());
                self.initiate_dkg().await?;
            }
        }
        Ok(())
    }

    async fn initiate_dkg(&mut self) -> Result<()> {
        let names: BTreeSet<XorName> = self.peers.keys().cloned().collect();
        let (keygen, message_and_target) =
            KeyGen::initialize(self.xor_name, names.len() - 1, names).unwrap();
        self.broadcast_dkg_messages(message_and_target).await?;

        self.keygen = Some(keygen);

        Ok(())
    }

    async fn handle_dkg_message(
        &mut self,
        message: bls_dkg::message::Message,
        rng: &mut impl RngCore,
    ) -> Result<()> {
        match &mut self.keygen {
            Some(keygen) => match keygen.handle_message(rng, message) {
                Ok(message_and_targets) => self.broadcast_dkg_messages(message_and_targets).await?,
                Err(e) => return Err(e).into_diagnostic(),
            },
            None => panic!("received dkg message before initiating dkg"),
        }

        match &mut self.keygen {
            Some(keygen) => {
                if keygen.is_finalized() {
                    let (_, outcome) = keygen.generate_keys().unwrap();
                    self.mint_node = Some(MintNode::new(SimpleKeyManager::from(
                        SimpleSigner::from(outcome),
                    )));
                    println!("DKG finalized!");
                    println!("MintNode created!");

                    self.listen_for_wallet_network_msgs().await?;
                }
                Ok(())
            }
            None => panic!("received dkg message before initiating dkg"),
        }
    }

    async fn broadcast_dkg_messages(
        &self,
        message_and_target: Vec<bls_dkg::key_gen::MessageAndTarget>,
    ) -> Result<()> {
        for (target, message) in message_and_target.into_iter() {
            let target_addr = self.peers.get(&target).unwrap();
            let msg = MintNetworkMsg::Dkg(message);
            self.send_mint_network_msg(&msg, target_addr).await?;
        }
        Ok(())
    }
}

/*
/// Displays mint information in human readable form
fn print_mintinfo_human(mintinfo: &MintInfo) -> Result<()> {
    println!();

    println!("Number of Mint Nodes: {}\n", mintinfo.mintnodes.len());

    println!("-- Mint Keys --\n");
    println!("SecretKeySet (Poly): {}\n", to_be_hex(&mintinfo.poly)?);

    println!(
        "PublicKeySet: {}\n",
        to_be_hex(&mintinfo.secret_key_set.public_keys())?
    );

    println!(
        "PublicKey: {}\n",
        to_be_hex(&mintinfo.secret_key_set.public_keys().public_key())?
    );

    println!("\n   -- SecretKeyShares --");
    for i in 0..mintinfo.secret_key_set.threshold() + 2 {
        println!(
            "    {}. {}",
            i,
            encode(&sks_to_bytes(&mintinfo.secret_key_set.secret_key_share(i))?)
        );
    }

    let mut secret_key_shares: BTreeMap<usize, SecretKeyShare> = Default::default();

    println!("\n   -- PublicKeyShares --");
    for i in 0..mintinfo.secret_key_set.threshold() + 2 {
        // the 2nd line matches ian coleman's bls tool output.  but why not the first?
        //        println!("  {}. {}", i, to_be_hex::<PublicKeyShare>(&sks.public_keys().public_key_share(i))?);
        println!(
            "    {}. {}",
            i,
            encode(
                &mintinfo
                    .secret_key_set
                    .public_keys()
                    .public_key_share(i)
                    .to_bytes()
            )
        );
        secret_key_shares.insert(i, mintinfo.secret_key_set.secret_key_share(i));
    }

    println!(
        "\n   Required Signers: {}   (Threshold = {})",
        mintinfo.secret_key_set.threshold() + 1,
        mintinfo.secret_key_set.threshold()
    );

    println!("\n-- Genesis DBC --\n");
    print_dbc_human(&mintinfo.genesis, true, None)?;

    for (i, spentbook) in mintinfo.spentbook_nodes.iter().enumerate() {
        println!("\n-- SpentBook Node {} --\n", i);
        for (key_image, _tx) in spentbook.iter() {
            println!("  {}", encode(&key_image.to_bytes()));
        }
    }

    println!();

    Ok(())
}

/// displays a welcome logo/banner for the app.
fn print_logo() {
    println!(
        r#"
 __     _
(_  _._|__  |\ | __|_     _ ._|
__)(_| |(/_ | \|(/_|_\/\/(_)| |<
 ____  ____   ____   __  __ _       _
|  _ \| __ ) / ___| |  \/  (_)_ __ | |_
| | | |  _ \| |     | |\/| | | '_ \| __|
| |_| | |_) | |___  | |  | | | | | | |_
|____/|____/ \____| |_|  |_|_|_| |_|\__|
  "#
    );
}
*/
