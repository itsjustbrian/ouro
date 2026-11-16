use std::{fmt::Display, path::PathBuf, str::FromStr, time::Duration};

use tokio::{fs, process::Command, sync::broadcast};
use tracing::{debug, error, info};

use crate::{
    GlobalArgs,
    firewall::{manage_firewall, prepare_firewall},
    port_manager::{Ports, map_ports},
};

fn ports_changed(current: &Ports, new: &Ports) -> bool {
    current != new
}

pub(crate) trait Service {
    const NAME: &str;

    async fn update(&self, ports: &Ports) -> eyre::Result<()>;
    async fn cleanup(&self) -> eyre::Result<()> {
        info!("Stopping slskd service...");
        Command::new("systemctl")
            .args(["stop", &format!("{}.service", Self::NAME)])
            .status()
            .await?;

        info!("Removing override configuration...");
        let override_dir = &format!("/run/systemd/system/{}.service.d", Self::NAME);
        let _ = fs::remove_dir_all(override_dir).await; // Ignore if doesn't exist

        Ok(())
    }
    // FIXME: ugly
    fn current_ports(&mut self) -> &mut Ports;

    async fn apply_ports(
        &mut self,
        interface: &str,
        ipv6: bool,
        ports: &Ports,
    ) -> eyre::Result<()> {
        let old_ports = self.current_ports();

        // Don't do anything if ports haven't changed
        if ports == old_ports {
            debug!("Ports unchanged, skipping update");
            return Ok(());
        }

        // First handle firewall
        manage_firewall(interface, ipv6, Some(ports)).await?;

        // Then update service
        self.update(ports).await?;

        // Update current ports
        *self.current_ports() = ports.clone();

        Ok(())
    }

    async fn run(
        &mut self,
        args: &GlobalArgs,
        mut shutdown: broadcast::Receiver<()>,
    ) -> eyre::Result<()> {
        prepare_firewall(&args.vpn_interface, args.ipv6).await?;
        let client = natpmp::new_tokio_natpmp_with(args.gateway).await?;
        let mut interval = tokio::time::interval(Duration::from_secs(45));

        // Initial port mapping
        info!("Requesting initial port mapping...");
        let initial_ports = map_ports(&client).await?;
        info!(
            "Got initial ports: TCP={}, UDP={}",
            initial_ports.tcp, initial_ports.udp
        );

        self.apply_ports(&args.vpn_interface, args.ipv6, &initial_ports)
            .await?;

        loop {
            tokio::select! {
                _ = interval.tick() => {
                    // Just refresh the NAT-PMP mapping without reconfiguring
                    match map_ports(&client).await {
                        Ok(new_ports) => {
                            let current_ports = self.current_ports().clone();
                            info!(
                                current_tcp = current_ports.tcp,
                                current_udp = current_ports.udp,
                                new_tcp = new_ports.tcp,
                                new_udp = new_ports.udp,
                                "Checking port mapping"
                            );

                            if ports_changed(&current_ports, &new_ports) {
                                info!("Ports changed, reconfiguring service");
                                self.apply_ports(&args.vpn_interface, args.ipv6, &new_ports).await?;
                            } else {
                                debug!("Ports unchanged, keeping current configuration");
                            }
                        }
                        Err(e) => {
                            error!("Port mapping refresh failed: {}", e);
                            tokio::time::sleep(Duration::from_secs(60)).await;
                        }
                    }
                }
                _ = shutdown.recv() => {
                    info!("Shutting down...");
                    manage_firewall(&args.vpn_interface, args.ipv6, None).await?;
                    self.cleanup().await?;
                    break;
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognizes_port_changes_including_return_to_initial_port() {
        let initial = Ports {
            tcp: 1000,
            udp: 1000,
        };
        let later = Ports {
            tcp: 2000,
            udp: 2000,
        };
        assert!(!ports_changed(&initial, &initial));
        assert!(ports_changed(&initial, &later));
        assert!(ports_changed(&later, &initial));
    }
}

pub enum ServiceData {
    Slskd(Slskd),
    Transmission(Transmission),
}

// FIXME: ugly
impl Service for ServiceData {
    const NAME: &str = "";

    async fn update(&self, ports: &Ports) -> eyre::Result<()> {
        match self {
            ServiceData::Slskd(slskd) => slskd.update(ports).await,
            ServiceData::Transmission(transmission) => transmission.update(ports).await,
        }
    }

    async fn cleanup(&self) -> eyre::Result<()> {
        match self {
            ServiceData::Slskd(slskd) => slskd.cleanup().await,
            ServiceData::Transmission(transmission) => transmission.cleanup().await,
        }
    }

    fn current_ports(&mut self) -> &mut Ports {
        match self {
            ServiceData::Slskd(slskd) => slskd.current_ports(),
            ServiceData::Transmission(transmission) => transmission.current_ports(),
        }
    }

    async fn apply_ports(
        &mut self,
        interface: &str,
        ipv6: bool,
        ports: &Ports,
    ) -> eyre::Result<()> {
        match self {
            ServiceData::Slskd(slskd) => slskd.apply_ports(interface, ipv6, ports).await,
            ServiceData::Transmission(transmission) => {
                transmission.apply_ports(interface, ipv6, ports).await
            }
        }
    }

    async fn run(
        &mut self,
        args: &GlobalArgs,
        shutdown: broadcast::Receiver<()>,
    ) -> eyre::Result<()> {
        match self {
            ServiceData::Slskd(slskd) => slskd.run(args, shutdown).await,
            ServiceData::Transmission(transmission) => transmission.run(args, shutdown).await,
        }
    }
}

pub struct Slskd {
    pub current_ports: Ports,
}

impl Service for Slskd {
    const NAME: &str = "slskd";

    fn current_ports(&mut self) -> &mut Ports {
        &mut self.current_ports
    }

    async fn update(&self, ports: &Ports) -> eyre::Result<()> {
        info!("Stopping slskd service...");
        Command::new("systemctl")
            .args(["stop", "slskd.service"])
            .status()
            .await?;

        let override_dir = PathBuf::from_str("/run/systemd/system/slskd.service.d")?;
        fs::create_dir_all(&override_dir).await?;

        fs::write(
            override_dir.join("override.conf"),
            format!(
                r#"
                    [Service]
                    Environment=SLSKD_SLSK_LISTEN_PORT={}
                "#,
                ports.tcp
            ),
        )
        .await?;

        Command::new("systemctl")
            .args(["daemon-reload"])
            .status()
            .await?;

        info!("Starting slskd with new configuration...");
        Command::new("systemctl")
            .args(["start", "slskd.service"])
            .status()
            .await?;

        Ok(())
    }
}

#[derive(Debug, Clone)]
pub struct Transmission {
    pub rpc_credentials: Option<Credentials>,
    pub address: Option<String>,
    pub current_ports: Ports,
}

#[derive(Debug, Clone)]
pub struct Credentials {
    pub username: String,
    pub password: String,
}

impl Display for Credentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}:{}", self.username, self.password)
    }
}

impl Service for Transmission {
    const NAME: &str = "transmission";

    async fn update(&self, ports: &Ports) -> eyre::Result<()> {
        let mut cmd = Command::new("transmission-remote");
        if let Some(addr) = &self.address {
            cmd.arg(addr);
        }

        // TODO: leaked in `ps`, pass through STDIN
        self.rpc_credentials
            .as_ref()
            .map(|rpc_credentials| cmd.args(["--auth", &rpc_credentials.to_string()]));
        let status = cmd
            .args(["--port", &format!("{}", ports.tcp)])
            .status()
            .await?;

        if !status.success() {
            eyre::bail!("transmission-remote failed with {status}");
        }

        Ok(())
    }

    fn current_ports(&mut self) -> &mut Ports {
        &mut self.current_ports
    }

    async fn cleanup(&self) -> eyre::Result<()> {
        Ok(())
    }
}
