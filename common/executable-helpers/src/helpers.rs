// Copyright (c) The Libra Core Contributors
// SPDX-License-Identifier: Apache-2.0

use libra_config::config::{NodeConfig, NodeConfigHelpers};
use libra_logger::prelude::*;
use slog_scope::GlobalLoggerGuard;
use std::path::Path;

pub fn load_config_from_path(config: Option<&Path>) -> NodeConfig {
    // Load the config
    let node_config = if let Some(path) = config {
        info!("Loading node config from: {}", path.display());
        NodeConfig::load(path).expect("NodeConfig")
    } else {
        info!("Loading test configs");
        NodeConfigHelpers::get_single_node_test_config(false /* random ports */)
    };

    // Node configuration contains important ephemeral port information and should
    // not be subject to being disabled as with other logs
    println!("Using node config {:?}", &node_config);

    node_config
}

pub fn setup_metrics(peer_id: &str, node_config: &NodeConfig) {
    let metrics_dir = node_config.get_metrics_dir();
    if !metrics_dir.as_os_str().is_empty() {
        libra_metrics::dump_all_metrics_to_file_periodically(
            &metrics_dir,
            &format!("{}.metrics", peer_id),
            node_config.metrics.collection_interval_ms,
        );
    }
}

pub fn setup_executable(
    config: Option<&Path>,
    no_logging: bool,
) -> (NodeConfig, Option<GlobalLoggerGuard>) {
    crash_handler::setup_panic_handler();
    let mut _logger = set_default_global_logger(no_logging, None);

    let config = load_config_from_path(config);

    // Reset the global logger using config (for chan_size currently).
    // We need to drop the global logger guard first before resetting it.
    _logger = None;
    let logger = set_default_global_logger(no_logging, Some(config.base.node_async_log_chan_size));
    for network in &config.networks {
        setup_metrics(&network.peer_id, &config);
    }

    (config, logger)
}

fn set_default_global_logger(
    is_logging_disabled: bool,
    chan_size: Option<usize>,
) -> Option<GlobalLoggerGuard> {
    if is_logging_disabled {
        return None;
    }

    Some(libra_logger::set_default_global_logger(
        true,      /* async */
        chan_size, /* chan_size */
    ))
}
