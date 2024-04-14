/*
 * Copyright (c) 2023 Stalwart Labs Ltd.
 *
 * This file is part of Stalwart Mail Server.
 *
 * This program is free software: you can redistribute it and/or modify
 * it under the terms of the GNU Affero General Public License as
 * published by the Free Software Foundation, either version 3 of
 * the License, or (at your option) any later version.
 *
 * This program is distributed in the hope that it will be useful,
 * but WITHOUT ANY WARRANTY; without even the implied warranty of
 * MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE. See the
 * GNU Affero General Public License for more details.
 * in the LICENSE file at the top-level directory of this distribution.
 * You should have received a copy of the GNU Affero General Public License
 * along with this program.  If not, see <http://www.gnu.org/licenses/>.
 *
 * You can be released from the requirements of the AGPLv3 license by
 * purchasing a commercial license. Please contact licensing@stalw.art
 * for more details.
*/

use std::path::PathBuf;

use arc_swap::ArcSwap;
use pwhash::sha512_crypt;
use store::{
    rand::{distributions::Alphanumeric, thread_rng, Rng},
    Stores,
};
use tracing_appender::non_blocking::WorkerGuard;
use utils::{
    config::{Config, ConfigKey},
    failed, UnwrapFailure,
};

use crate::{
    config::{server::Servers, tracers::Tracers},
    manager::SPAMFILTER_URL,
    Core, SharedCore,
};

use super::{
    config::{ConfigManager, Patterns},
    download_resource, WEBADMIN_KEY, WEBADMIN_URL,
};

pub struct BootManager {
    pub config: Config,
    pub core: SharedCore,
    pub servers: Servers,
    pub guards: Option<Vec<WorkerGuard>>,
}

impl BootManager {
    pub async fn init(optional_config_path: Option<String>) -> Self {
        let mut config_path;

        if optional_config_path.is_some() {
            config_path = optional_config_path;
        } else {
            config_path = std::env::var("CONFIG_PATH").ok();

            if config_path.is_none() {
                let mut args = std::env::args().skip(1);

                if let Some(arg) = args
                    .next()
                    .and_then(|arg| arg.strip_prefix("--").map(|arg| arg.to_string()))
                {
                    let (key, value) = if let Some((key, value)) = arg.split_once('=') {
                        (key.to_string(), value.trim().to_string())
                    } else if let Some(value) = args.next() {
                        (arg, value)
                    } else {
                        failed(&format!("Invalid command line argument: {arg}"));
                    };

                    match key.as_str() {
                        "config" => {
                            config_path = Some(value);
                        }
                        "init" => {
                            quickstart(value);
                            std::process::exit(0);
                        }
                        _ => {
                            failed(&format!("Invalid command line argument: {key}"));
                        }
                    }
                }
            }
        }

        // Read main configuration file
        let cfg_local_path =
            PathBuf::from(config_path.failed("Missing parameter --config=<path-to-config>."));
        let mut config = Config::default();
        match std::fs::read_to_string(&cfg_local_path) {
            Ok(value) => {
                config.parse(&value).failed("Invalid configuration file");
            }
            Err(err) => {
                config.new_build_error("*", format!("Could not read configuration file: {err}"));
            }
        }
        let cfg_local = config.keys.clone();

        // Resolve macros
        config.resolve_macros().await;

        // Parser servers
        let mut servers = Servers::parse(&mut config);

        // Bind ports and drop privileges
        servers.bind_and_drop_priv(&mut config);

        // Load stores
        let mut stores = Stores::parse(&mut config).await;

        // Build manager
        let manager = ConfigManager {
            cfg_local: ArcSwap::from_pointee(cfg_local),
            cfg_local_path,
            cfg_local_patterns: Patterns::parse(&mut config).into(),
            cfg_store: config
                .value("storage.data")
                .and_then(|id| stores.stores.get(id))
                .cloned()
                .unwrap_or_default(),
        };

        // Extend configuration with settings stored in the db
        if !manager.cfg_store.is_none() {
            manager
                .extend_config(&mut config, "")
                .await
                .failed("Failed to read configuration");
        }

        // Enable tracing
        let guards = Tracers::parse(&mut config).enable(&mut config);
        tracing::info!(
            "Starting Stalwart Mail Server v{}...",
            env!("CARGO_PKG_VERSION")
        );

        // Add hostname lookup if missing
        let mut insert_keys = Vec::new();
        if config
            .value("lookup.default.hostname")
            .filter(|v| !v.is_empty())
            .is_none()
        {
            insert_keys.push(ConfigKey::from((
                "lookup.default.hostname",
                hostname::get()
                    .map(|v| v.to_string_lossy().into_owned())
                    .unwrap_or_else(|_| "localhost".to_string()),
            )));
        }

        // Generate an OAuth key if missing
        if config
            .value("oauth.key")
            .filter(|v| !v.is_empty())
            .is_none()
        {
            insert_keys.push(ConfigKey::from((
                "oauth.key",
                thread_rng()
                    .sample_iter(Alphanumeric)
                    .take(64)
                    .map(char::from)
                    .collect::<String>(),
            )));
        }

        // Download SPAM filters if missing
        if config
            .value("version.spam-filter")
            .filter(|v| !v.is_empty())
            .is_none()
        {
            match manager.fetch_external_config(SPAMFILTER_URL).await {
                Ok(external_config) => {
                    tracing::info!(
                        context = "config",
                        event = "import",
                        url = SPAMFILTER_URL,
                        version = external_config.version,
                        "Imported spam filter rules"
                    );
                    insert_keys.extend(external_config.keys);
                }
                Err(err) => {
                    config.new_build_error("*", format!("Failed to fetch spam filter: {err}"));
                }
            }

            // Add default settings
            for key in [
                ("queue.quota.size.messages", "100000"),
                ("queue.quota.size.size", "10737418240"),
                ("queue.quota.size.enable", "true"),
                ("queue.throttle.rcpt.key", "rcpt_domain"),
                ("queue.throttle.rcpt.concurrency", "5"),
                ("queue.throttle.rcpt.enable", "true"),
                ("session.throttle.ip.key", "remote_ip"),
                ("session.throttle.ip.concurrency", "5"),
                ("session.throttle.ip.enable", "true"),
                ("session.throttle.sender.key.0", "sender_domain"),
                ("session.throttle.sender.key.1", "rcpt"),
                ("session.throttle.sender.rate", "25/1h"),
                ("session.throttle.sender.enable", "true"),
                ("report.analysis.addresses", "postmaster@*"),
            ] {
                insert_keys.push(ConfigKey::from(key));
            }
        }

        // Download webadmin if missing
        if let Some(blob_store) = config
            .value("storage.blob")
            .and_then(|id| stores.blob_stores.get(id))
        {
            match blob_store.get_blob(WEBADMIN_KEY, 0..usize::MAX).await {
                Ok(Some(_)) => (),
                Ok(None) => match download_resource(WEBADMIN_URL).await {
                    Ok(bytes) => match blob_store.put_blob(WEBADMIN_KEY, &bytes).await {
                        Ok(_) => {
                            tracing::info!(
                                context = "webadmin",
                                event = "download",
                                url = WEBADMIN_URL,
                                "Downloaded webadmin bundle"
                            );
                        }
                        Err(err) => {
                            config.new_build_error(
                                "*",
                                format!("Failed to store webadmin blob: {err}"),
                            );
                        }
                    },
                    Err(err) => {
                        config.new_build_error("*", format!("Failed to download webadmin: {err}"));
                    }
                },
                Err(err) => {
                    config.new_build_error("*", format!("Failed to access webadmin blob: {err}"))
                }
            }
        }

        // Add missing settings
        if !insert_keys.is_empty() {
            for item in &insert_keys {
                config.keys.insert(item.key.clone(), item.value.clone());
            }

            if let Err(err) = manager.set(insert_keys).await {
                config.new_build_error("*", format!("Failed to update configuration: {err}"));
            }
        }

        // Parse lookup stores
        stores.parse_lookups(&mut config).await;

        // Parse settings and build shared core
        let core = Core::parse(&mut config, stores, manager)
            .await
            .into_shared();

        // Parse TCP acceptors
        servers.parse_tcp_acceptors(&mut config, core.clone());

        BootManager {
            core,
            guards,
            config,
            servers,
        }
    }
}

fn quickstart(path: impl Into<PathBuf>) {
    let path = path.into();

    if !path.exists() {
        std::fs::create_dir_all(&path).failed("Failed to create directory");
    }

    for dir in &["etc", "data", "logs"] {
        let sub_path = path.join(dir);
        if !sub_path.exists() {
            std::fs::create_dir(sub_path).failed(&format!("Failed to create {dir} directory"));
        }
    }

    let admin_pass = std::env::var("STALWART_ADMIN_PASSWORD").unwrap_or_else(|_| {
        thread_rng()
            .sample_iter(Alphanumeric)
            .take(10)
            .map(char::from)
            .collect::<String>()
    });

    std::fs::write(
        path.join("etc").join("config.toml"),
        QUICKSTART_CONFIG
            .replace("_P_", &path.to_string_lossy())
            .replace("_S_", &sha512_crypt::hash(&admin_pass).unwrap()),
    )
    .failed("Failed to write configuration file");

    eprintln!(
        "✅ Configuration file written to {}/etc/config.toml",
        path.to_string_lossy()
    );
    eprintln!("🔑 Your administrator account is 'admin' with password '{admin_pass}'.");
}

#[cfg(not(feature = "foundation"))]
const QUICKSTART_CONFIG: &str = r#"[server.listener.smtp]
bind = "[::]:25"
protocol = "smtp"

[server.listener.submission]
bind = "[::]:587"
protocol = "smtp"

[server.listener.submissions]
bind = "[::]:465"
protocol = "smtp"
tls.implicit = true

[server.listener.imap]
bind = "[::]:143"
protocol = "imap"

[server.listener.imaptls]
bind = "[::]:993"
protocol = "imap"
tls.implicit = true

[server.listener.sieve]
bind = "[::]:4190"
protocol = "managesieve"

[server.listener.https]
protocol = "http"
bind = "[::]:443"
tls.implicit = true

[server.listener.http]
protocol = "http"
bind = "[::]:8080"

[storage]
data = "rocksdb"
fts = "rocksdb"
blob = "rocksdb"
lookup = "rocksdb"
directory = "internal"

[store.rocksdb]
type = "rocksdb"
path = "_P_/data"
compression = "lz4"

[directory.internal]
type = "internal"
store = "rocksdb"

[tracer.log]
type = "log"
level = "info"
path = "_P_/logs"
prefix = "stalwart.log"
rotate = "daily"
ansi = false
enable = true

[authentication.fallback-admin]
user = "admin"
secret = "_S_"
"#;

#[cfg(feature = "foundation")]
const QUICKSTART_CONFIG: &str = r#"[server.listener.smtp]
bind = "[::]:25"
protocol = "smtp"

[server.listener.submission]
bind = "[::]:587"
protocol = "smtp"

[server.listener.submissions]
bind = "[::]:465"
protocol = "smtp"
tls.implicit = true

[server.listener.imap]
bind = "[::]:143"
protocol = "imap"

[server.listener.imaptls]
bind = "[::]:993"
protocol = "imap"
tls.implicit = true

[server.listener.sieve]
bind = "[::]:4190"
protocol = "managesieve"

[server.listener.https]
protocol = "http"
bind = "[::]:443"
tls.implicit = true

[server.listener.http]
protocol = "http"
bind = "[::]:8080"

[storage]
data = "foundation-db"
fts = "foundation-db"
blob = "foundation-db"
lookup = "foundation-db"
directory = "internal"

[store.foundation-db]
type = "foundationdb"
compression = "lz4"

[directory.internal]
type = "internal"
store = "foundation-db"

[tracer.log]
type = "log"
level = "info"
path = "_P_/logs"
prefix = "stalwart.log"
rotate = "daily"
ansi = false
enable = true

[authentication.fallback-admin]
user = "admin"
secret = "_S_"
"#;
