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

use std::{
    sync::Arc,
    time::{Duration, Instant},
};

use mail_auth::{IpLookupStrategy, MX};
use utils::config::{Config, ServerProtocol};

use crate::smtp::{
    inbound::TestQueueEvent, outbound::start_test_server, session::TestSession, TestConfig,
    TestSMTP,
};
use smtp::{
    config::IfBlock,
    core::{Session, SMTP},
    queue::{manager::Queue, DeliveryAttempt},
};
use smtp::config::resolver::ConfigResolver;

#[tokio::test]
#[serial_test::serial]
async fn ip_lookup_strategy() {
    /*tracing::subscriber::set_global_default(
        tracing_subscriber::FmtSubscriber::builder()
            .with_max_level(tracing::Level::TRACE)
            .finish(),
    )
    .unwrap();*/

    // Start test server
    let mut core = SMTP::test();
    core.session.config.rcpt.relay = IfBlock::new(true);
    let mut remote_qr = core.init_test_queue("smtp_iplookup_remote");
    let _rx = start_test_server(core.into(), &[ServerProtocol::Smtp]);

    for strategy in [IpLookupStrategy::Ipv6Only, IpLookupStrategy::Ipv6thenIpv4] {
        println!("-> Strategy: {:?}", strategy);
        // Add mock DNS entries
        let mut core = SMTP::test();
        core.queue.config.ip_strategy = IfBlock::new(IpLookupStrategy::Ipv6thenIpv4);
        core.resolvers.dns.mx_add(
            "foobar.org",
            vec![MX {
                exchanges: vec!["mx.foobar.org".to_string()],
                preference: 10,
            }],
            Instant::now() + Duration::from_secs(10),
        );
        if matches!(strategy, IpLookupStrategy::Ipv6thenIpv4) {
            core.resolvers.dns.ipv4_add(
                "mx.foobar.org",
                vec!["127.0.0.1".parse().unwrap()],
                Instant::now() + Duration::from_secs(10),
            );
        }
        core.resolvers.dns.ipv6_add(
            "mx.foobar.org",
            vec!["::1".parse().unwrap()],
            Instant::now() + Duration::from_secs(10),
        );

        // Retry on failed STARTTLS
        let mut local_qr = core.init_test_queue("smtp_iplookup_local");
        core.session.config.rcpt.relay = IfBlock::new(true);

        let core = Arc::new(core);
        let mut queue = Queue::default();
        let mut session = Session::test(core.clone());
        session.data.remote_ip = "10.0.0.1".parse().unwrap();
        session.eval_session_params().await;
        session.ehlo("mx.test.org").await;
        session
            .send_message("john@test.org", &["bill@foobar.org"], "test:no_dkim", "250")
            .await;
        DeliveryAttempt::from(local_qr.read_event().await.unwrap_message())
            .try_deliver(core.clone(), &mut queue)
            .await;
        if matches!(strategy, IpLookupStrategy::Ipv6thenIpv4) {
            local_qr.read_event().await.unwrap_done();
            remote_qr.read_event().await.unwrap_message();
        } else {
            let status = local_qr.read_event().await.unwrap_retry().inner.domains[0]
                .status
                .to_string();
            assert!(status.contains("Connection refused"));
        }
    }
}

#[tokio::test]
async fn custom_nameserver_config() {
    let mut config = Config::default();
    config.parse(r#"
[resolver]
type = "custom"
concurrency = 1
timeout = "1s"
attempts = 1
try-tcp-on-error = false
public-suffix = []

[[resolver.nameservers]]
ip = "1.1.1.1"
port = 53
protocol = "udp"
"#).unwrap();
    config.build_resolvers().unwrap();
}

#[tokio::test]
async fn custom_nameserver_config_missing() {
    let mut config = Config::default();
    config.parse(r#"
[resolver]
type = "custom"
concurrency = 1
timeout = "1s"
attempts = 1
try-tcp-on-error = false
public-suffix = []
"#).unwrap();

    // should error, as we must have at least one nameserver configured
    match config.build_resolvers() {
        Ok(_) => panic!("Expected building resolvers to fail."),
        Err(e) => assert_eq!(e, "No custom resolver nameservers configured."),
    }
}