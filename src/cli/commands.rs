use crate::cli::prompts::{
    prompt_for_farming_fullnode, prompt_for_farming_port, prompt_for_launcher_id,
    prompt_for_mnemonic, prompt_for_payout_address, prompt_for_plot_directories,
    prompt_for_rpc_fullnode, prompt_for_rpc_port, prompt_for_ssl_path,
};
use crate::cli::utils::{init_logger, rpc_client_from_config};
use crate::farmer::config::{load_keys, Config, DruidGardenHarvesterConfig, FarmingInfo};
use crate::farmer::{ExtendedFarmerSharedState, Farmer};
use crate::metrics::Metrics;
use crate::tasks::blockchain_state_updater::update_blockchain;
use crate::tasks::pool_state_updater::pool_updater;
use crate::{gui, metrics, HEADERS};
use actix_web::web::Data;
use actix_web::{App, HttpServer};
use dg_xch_cli::wallet_commands::migrate_plot_nft;
use dg_xch_cli::wallets::plotnft_utils::{get_plotnft_by_launcher_id, scrounge_for_plotnfts};
use dg_xch_clients::api::pool::DefaultPoolClient;
use dg_xch_clients::rpc::full_node::FullnodeClient;
use dg_xch_core::blockchain::sized_bytes::{Bytes32, Bytes48};
use dg_xch_core::config::PoolWalletConfig;
use dg_xch_core::consensus::constants::{CONSENSUS_CONSTANTS_MAP, MAINNET};
use dg_xch_core::plots::PlotNft;
use dg_xch_core::protocols::farmer::{FarmerMetrics, FarmerSharedState};
use dg_xch_core::utils::await_termination;
use dg_xch_keys::{
    key_from_mnemonic, master_sk_to_farmer_sk, master_sk_to_pool_sk,
    master_sk_to_pooling_authentication_sk, master_sk_to_singleton_owner_sk,
    master_sk_to_wallet_sk, master_sk_to_wallet_sk_unhardened, parse_payout_address,
};
use dg_xch_puzzles::clvm_puzzles::launcher_id_to_p2_puzzle_hash;
use dg_xch_puzzles::p2_delegated_puzzle_or_hidden_puzzle::puzzle_hash_for_pk;
use dialoguer::Confirm;
use hex::encode;
use log::{info, warn};
use std::collections::HashMap;
use std::io::{Error, ErrorKind};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tokio::join;
use tokio::sync::RwLock;
use tokio::task::JoinHandle;

pub(crate) async fn tui_mode(config_path: &Path) -> Result<(), Error> {
    let config = Config::try_from(config_path).unwrap_or_default();
    let config_arc = Arc::new(config);
    gui::bootstrap(config_arc).await?;
    Ok(())
}

pub(crate) async fn cli_mode(config_path: &Path) -> Result<(), Error> {
    init_logger();
    let config = Config::try_from(config_path)?;
    let config_arc = Arc::new(config);
    let constants = CONSENSUS_CONSTANTS_MAP
        .get(&config_arc.selected_network)
        .unwrap_or(&MAINNET);
    info!(
        "Selected Network: {}, AggSig: {}",
        &config_arc.selected_network,
        &encode(&constants.agg_sig_me_additional_data)
    );
    let (farmer_private_keys, owner_secret_keys, auth_secret_keys, pool_public_keys) =
        load_keys(config_arc.clone()).await;
    let extended_metrics = Arc::new(Metrics::default());
    let farmer_metrics = FarmerMetrics::new(&*extended_metrics.registry.read().await);
    let shared_state = Arc::new(FarmerSharedState {
        farmer_private_keys: Arc::new(farmer_private_keys),
        owner_secret_keys: Arc::new(owner_secret_keys),
        owner_public_keys_to_auth_secret_keys: Arc::new(auth_secret_keys),
        pool_public_keys: Arc::new(pool_public_keys),
        data: Arc::new(ExtendedFarmerSharedState {
            config: config_arc.clone(),
            run: Arc::new(AtomicBool::new(true)),
            extended_metrics,
            ..Default::default()
        }),
        metrics: Arc::new(RwLock::new(Some(farmer_metrics))),
        ..Default::default()
    });

    info!("Using Additional Headers: {:?}", &*HEADERS);
    //Pool Updater vars
    let pool_state = shared_state.clone();
    let pool_state_handle: JoinHandle<()> =
        tokio::spawn(async move { pool_updater(pool_state).await });

    //Signal Handler to shut down the Async processes
    let signal_run = shared_state.data.run.clone();
    let signal_handle = tokio::spawn(async move {
        let _ = await_termination().await;
        signal_run.store(false, Ordering::Relaxed);
    });

    let pool_client = Arc::new(DefaultPoolClient::new());
    let farmer = Farmer::new(shared_state.clone(), pool_client).await?;
    //Client Vars
    let client_handle: JoinHandle<Result<(), Error>> = tokio::spawn(async move {
        farmer.run().await;
        Ok(())
    });
    let fn_shared_state = shared_state.clone();
    let fullnode_thread = tokio::spawn(async move {
        update_blockchain(config_arc.clone(), fn_shared_state.clone()).await
    });
    let metrics = shared_state.data.config.metrics.clone().unwrap_or_default();
    info!("Metrics: {} on port: {}", metrics.enabled, metrics.port);
    let server_handle = if metrics.enabled {
        tokio::spawn(
            HttpServer::new(move || {
                App::new()
                    .app_data(Data::new(shared_state.clone()))
                    .configure(metrics::init)
            })
            .bind(("0.0.0.0", metrics.port))?
            .run(),
        )
    } else {
        tokio::spawn(async { Ok::<(), Error>(()) })
    };
    let _ = join!(
        pool_state_handle,
        client_handle,
        signal_handle,
        server_handle,
        fullnode_thread
    );
    Ok(())
}

pub struct GenerateConfig {
    pub output_path: Option<PathBuf>,
    pub mnemonic_file: Option<String>,
    pub fullnode_ws_host: Option<String>,
    pub fullnode_ws_port: Option<u16>,
    pub fullnode_rpc_host: Option<String>,
    pub fullnode_rpc_port: Option<u16>,
    pub fullnode_ssl: Option<String>,
    pub network: Option<String>,
    pub launcher_id: Option<Bytes32>,
    pub payout_address: Option<String>,
    pub plot_directories: Option<Vec<String>>,
    pub additional_headers: Option<HashMap<String, String>>,
}

pub async fn generate_config_from_mnemonic(gen_settings: GenerateConfig) -> Result<Config, Error> {
    if let Some(op) = &gen_settings.output_path {
        if op.exists()
            && !Confirm::new()
                .with_prompt(format!(
                    "An existing config exists at {:?}, would you like to override it? (Y/N)",
                    op
                ))
                .interact()
                .map_err(|e| {
                    Error::new(
                        ErrorKind::Interrupted,
                        format!("Dialog Interrupted: {:?}", e),
                    )
                })?
        {
            return Err(Error::new(ErrorKind::Interrupted, "User Canceled"));
        }
    }
    let mut config = Config::default();
    let network = gen_settings
        .network
        .map(|v| {
            if CONSENSUS_CONSTANTS_MAP.contains_key(&v) {
                v
            } else {
                "mainnet".to_string()
            }
        })
        .unwrap_or("mainnet".to_string());
    config.selected_network = network;
    let master_key = key_from_mnemonic(&prompt_for_mnemonic(gen_settings.mnemonic_file)?)?;
    config.payout_address = prompt_for_payout_address(gen_settings.payout_address)?.to_string();
    config.fullnode_ws_host =
        prompt_for_farming_fullnode(gen_settings.fullnode_ws_host)?.to_string();
    config.fullnode_rpc_host = if let Some(host) = gen_settings.fullnode_rpc_host {
        host
    } else if "chia-proxy.evergreenminer-prod.com" == config.fullnode_ws_host {
        "chia-proxy.evergreenminer-prod.com".to_string()
    } else {
        prompt_for_rpc_fullnode(None)?
    };
    config.fullnode_ws_port = if let Some(port) = gen_settings.fullnode_ws_port {
        port
    } else if "chia-proxy.evergreenminer-prod.com" == config.fullnode_ws_host {
        443
    } else {
        8444
    };
    config.fullnode_rpc_port = if let Some(port) = gen_settings.fullnode_rpc_port {
        port
    } else if "chia-proxy.evergreenminer-prod.com" == config.fullnode_rpc_host {
        443
    } else {
        8555
    };
    config.ssl_root_path = if "chia-proxy.evergreenminer-prod.com" == config.fullnode_ws_host {
        None
    } else {
        prompt_for_ssl_path(gen_settings.fullnode_ssl)?
    };
    config.harvester_configs.druid_garden = Some(DruidGardenHarvesterConfig {
        plot_directories: if let Some(dirs) = gen_settings.plot_directories {
            dirs
        } else {
            prompt_for_plot_directories()?
        },
    });
    let client = rpc_client_from_config(&config, &gen_settings.additional_headers);
    let mut page = 0;
    let mut plotnfts = vec![];
    if let Some(launcher_id) = prompt_for_launcher_id(gen_settings.launcher_id)? {
        info!("Searching for NFT with LauncherID: {launcher_id}");
        if let Some(plotnft) =
            get_plotnft_by_launcher_id(client.clone(), &launcher_id, None).await?
        {
            plotnfts.push(plotnft);
        } else {
            return Err(Error::new(
                ErrorKind::NotFound,
                "Failed to find a plotNFT with LauncherID: {launcher_id}",
            ));
        }
    } else {
        info!("No LauncherID Specified, Searching for PlotNFTs...");
        while page < 50 && plotnfts.is_empty() {
            let mut puzzle_hashes = vec![];
            for index in page * 50..(page + 1) * 50 {
                let wallet_sk =
                    master_sk_to_wallet_sk_unhardened(&master_key, index).map_err(|e| {
                        Error::new(
                            ErrorKind::InvalidInput,
                            format!("Failed to parse Wallet SK: {:?}", e),
                        )
                    })?;
                let pub_key: Bytes48 = wallet_sk.sk_to_pk().to_bytes().into();
                puzzle_hashes.push(puzzle_hash_for_pk(&pub_key)?);
                let hardened_wallet_sk =
                    master_sk_to_wallet_sk(&master_key, index).map_err(|e| {
                        Error::new(
                            ErrorKind::InvalidInput,
                            format!("Failed to parse Wallet SK: {:?}", e),
                        )
                    })?;
                let pub_key: Bytes48 = hardened_wallet_sk.sk_to_pk().to_bytes().into();
                puzzle_hashes.push(puzzle_hash_for_pk(&pub_key)?);
            }
            plotnfts.extend(scrounge_for_plotnfts(client.clone(), &puzzle_hashes).await?);
            page += 1;
        }
    }
    for plot_nft in plotnfts {
        config.pool_info.push(PoolWalletConfig {
            launcher_id: plot_nft.launcher_id,
            pool_url: plot_nft.pool_state.pool_url.unwrap_or_default(),
            target_puzzle_hash: plot_nft.pool_state.target_puzzle_hash,
            payout_instructions: config.payout_address.clone(),
            p2_singleton_puzzle_hash: launcher_id_to_p2_puzzle_hash(
                &plot_nft.launcher_id,
                plot_nft.delay_time as u64,
                &plot_nft.delay_puzzle_hash,
            )?,
            owner_public_key: plot_nft.pool_state.owner_pubkey,
            difficulty: None,
        });
        let mut owner_key = None;
        let mut auth_key = None;
        for i in 0..150 {
            let key = master_sk_to_singleton_owner_sk(&master_key, i).unwrap();
            let pub_key: Bytes48 = key.sk_to_pk().to_bytes().into();
            if pub_key == plot_nft.pool_state.owner_pubkey {
                let a_key = master_sk_to_pooling_authentication_sk(&master_key, i, 0).unwrap();
                owner_key = Some(key.into());
                auth_key = Some(a_key.into());
                break;
            }
        }
        if let Some(info) = config.farmer_info.iter_mut().find(|f| {
            if let Some(l) = &f.launcher_id {
                l == &plot_nft.launcher_id
            } else {
                false
            }
        }) {
            info.farmer_secret_key = master_sk_to_farmer_sk(&master_key)?.into();
            info.launcher_id = Some(plot_nft.launcher_id);
            info.pool_secret_key = Some(master_sk_to_pool_sk(&master_key)?.into());
            info.owner_secret_key = owner_key;
            info.auth_secret_key = auth_key;
        } else {
            config.farmer_info.push(FarmingInfo {
                farmer_secret_key: master_sk_to_farmer_sk(&master_key)?.into(),
                launcher_id: Some(plot_nft.launcher_id),
                pool_secret_key: Some(master_sk_to_pool_sk(&master_key)?.into()),
                owner_secret_key: owner_key,
                auth_secret_key: auth_key,
            });
        }
    }
    if config.farmer_info.is_empty() {
        warn!("No PlotNFT Found");
        config.farmer_info.push(FarmingInfo {
            farmer_secret_key: master_sk_to_farmer_sk(&master_key)?.into(),
            launcher_id: None,
            pool_secret_key: Some(master_sk_to_pool_sk(&master_key)?.into()),
            owner_secret_key: None,
            auth_secret_key: None,
        });
    }
    if let Some(op) = &gen_settings.output_path {
        config.save_as_yaml(op)?;
    }
    Ok(config)
}

pub async fn update_pool_info(
    config: Config,
    launcher_id: Option<String>,
) -> Result<Config, Error> {
    let client = rpc_client_from_config(&config, &None);
    #[inline]
    async fn handle_launcher_id(
        plot_nfts: &mut Vec<PlotNft>,
        client: Arc<FullnodeClient>,
        launcher_id: Bytes32,
    ) -> Result<(), Error> {
        info!(
            "Fetching current PlotNFT state for launcher id {} ...",
            launcher_id.to_string()
        );
        plot_nfts.extend(get_plotnft_by_launcher_id(client.clone(), &launcher_id, None).await?);
        Ok(())
    }
    let mut plot_nfts = vec![];
    for farmer_info in &config.farmer_info {
        if let Some(farmer_launcher_id) = farmer_info.launcher_id {
            if let Some(input_launcher_id) = &launcher_id {
                if Bytes32::from(input_launcher_id) == farmer_launcher_id {
                    handle_launcher_id(&mut plot_nfts, client.clone(), farmer_launcher_id).await?;
                }
            } else {
                handle_launcher_id(&mut plot_nfts, client.clone(), farmer_launcher_id).await?;
            }
        }
    }
    if plot_nfts.is_empty() && launcher_id.is_some() {
        return Err(Error::new(
            ErrorKind::InvalidInput,
            format!(
                "Failed to Find a PlotNFT with Launcher ID: {}",
                launcher_id.expect("Checked Some Above")
            ),
        ));
    }
    let mut updated_config = config.clone();
    for plot_nft in plot_nfts {
        if let Some(pool_wallet) = updated_config
            .pool_info
            .iter_mut()
            .find(|pw| pw.launcher_id == plot_nft.launcher_id)
        {
            let old_pool_wallet = pool_wallet.clone();
            pool_wallet.pool_url = plot_nft.pool_state.pool_url.unwrap_or_default();
            if pool_wallet.target_puzzle_hash != plot_nft.pool_state.target_puzzle_hash {
                // Reset diff on pool change
                pool_wallet.difficulty = None;
            }
            pool_wallet.target_puzzle_hash = plot_nft.pool_state.target_puzzle_hash;
            pool_wallet.owner_public_key = plot_nft.pool_state.owner_pubkey;

            let mut change_messages: Vec<String> = vec![];
            if old_pool_wallet.pool_url != pool_wallet.pool_url {
                change_messages.push(format!(
                    "from {} to {}",
                    old_pool_wallet.pool_url, pool_wallet.pool_url
                ));
            }
            if old_pool_wallet.target_puzzle_hash != pool_wallet.target_puzzle_hash {
                change_messages.push(format!(
                    "from PH {} to PH {}",
                    old_pool_wallet.target_puzzle_hash, pool_wallet.target_puzzle_hash
                ));
            }
            if change_messages.is_empty() {
                info!(
                    "PlotNFT state for launcher id {} did not change",
                    plot_nft.launcher_id.to_string(),
                );
            } else {
                info!(
                    "PlotNFT state for launcher id {} did change {}",
                    plot_nft.launcher_id.to_string(),
                    change_messages.join(" and "),
                );
            }
        }
    }
    Ok(updated_config)
}

pub async fn join_pool(
    config: Config,
    pool_url: String,
    mnemonic_file: Option<String>,
    launcher_id: Option<String>,
    fee: Option<u64>,
) -> Result<Config, Error> {
    let client = rpc_client_from_config(&config, &None);
    let mnemonic = prompt_for_mnemonic(mnemonic_file)?;
    let mut found = false;
    let owner_ph = Bytes32::from(parse_payout_address(&config.payout_address)?);
    for farmer_info in &config.farmer_info {
        if let Some(farmer_launcher_id) = farmer_info.launcher_id {
            if let Some(selected_launcher_id) = &launcher_id {
                if Bytes32::from(selected_launcher_id) == farmer_launcher_id {
                    migrate_plot_nft(
                        client.clone(),
                        &pool_url,
                        &farmer_launcher_id,
                        &owner_ph,
                        &mnemonic.to_string(),
                        CONSENSUS_CONSTANTS_MAP
                            .get(&config.selected_network)
                            .cloned()
                            .unwrap_or(MAINNET.clone()),
                        fee.unwrap_or_default(),
                    )
                    .await?;
                    found = true;
                }
            } else {
                migrate_plot_nft(
                    client.clone(),
                    &pool_url,
                    &farmer_launcher_id,
                    &owner_ph,
                    &mnemonic.to_string(),
                    CONSENSUS_CONSTANTS_MAP
                        .get(&config.selected_network)
                        .cloned()
                        .unwrap_or(MAINNET.clone()),
                    fee.unwrap_or_default(),
                )
                .await?;
                found = true;
            }
        }
    }
    if !found && launcher_id.is_some() {
        return Err(Error::new(
            ErrorKind::InvalidInput,
            format!(
                "Failed to Find a PlotNFT with Launcher ID: {}",
                launcher_id.expect("Checked Some Above")
            ),
        ));
    }
    update_pool_info(config, launcher_id).await
}
pub async fn update(config: Config) -> Result<Config, Error> {
    let mut config = config;
    config.payout_address =
        prompt_for_payout_address(Some(config.payout_address.clone()))?.to_string();
    config.fullnode_ws_host =
        prompt_for_farming_fullnode(Some(config.fullnode_ws_host.clone()))?.to_string();
    config.fullnode_rpc_host =
        prompt_for_rpc_fullnode(Some(config.fullnode_ws_host.clone()))?.to_string();
    config.fullnode_ws_port = prompt_for_farming_port(
        if "chia-proxy.evergreenminer-prod.com" == config.fullnode_ws_host {
            Some(443)
        } else {
            Some(config.fullnode_ws_port)
        },
    )?;
    config.fullnode_rpc_port = prompt_for_rpc_port(
        if "chia-proxy.evergreenminer-prod.com" == config.fullnode_rpc_host {
            Some(443)
        } else {
            Some(config.fullnode_rpc_port)
        },
    )?;
    config.ssl_root_path = prompt_for_ssl_path(config.ssl_root_path)?;
    config.harvester_configs.druid_garden = Some(DruidGardenHarvesterConfig {
        plot_directories: if let Some(gh) = config.harvester_configs.druid_garden {
            gh.plot_directories
        } else {
            prompt_for_plot_directories()?
        },
    });
    update_pool_info(config, None).await
}
