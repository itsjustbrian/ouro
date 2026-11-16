use std::process::Stdio;

use eyre::{Context, bail};
use tokio::process::Command;
use tracing::info;

use crate::port_manager::Ports;

// The namespace is shared with other services, so only touch our own chain.
fn chain_name(interface: &str) -> String {
    format!("OURO-{interface}")
}

async fn run(program: &str, args: &[&str]) -> eyre::Result<bool> {
    let status = Command::new(program)
        .args(args)
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .status()
        .await
        .wrap_err_with(|| format!("failed to execute {program}"))?;
    Ok(status.success())
}

async fn checked(program: &str, args: &[&str]) -> eyre::Result<()> {
    if !run(program, args).await? {
        bail!("{program} {} failed", args.join(" "));
    }
    Ok(())
}

async fn prepare(program: &str, interface: &str) -> eyre::Result<()> {
    let chain = chain_name(interface);
    // A previous instance may have exited without cleaning up. Flush its
    // chain before obtaining a new lease so stale ports do not remain open.
    if !run(program, &["-S", &chain]).await? {
        checked(program, &["-N", &chain]).await?;
    }
    checked(program, &["-F", &chain]).await?;
    if !run(program, &["-C", "INPUT", "-i", interface, "-j", &chain]).await? {
        checked(program, &["-A", "INPUT", "-i", interface, "-j", &chain]).await?;
    }
    Ok(())
}

pub async fn prepare_firewall(interface: &str, ipv6: bool) -> eyre::Result<()> {
    prepare("iptables", interface).await?;
    if ipv6 {
        prepare("ip6tables", interface).await?;
    }
    Ok(())
}

async fn update(program: &str, interface: &str, ports: Option<&Ports>) -> eyre::Result<()> {
    let chain = chain_name(interface);
    checked(program, &["-F", &chain]).await?;
    if let Some(ports) = ports {
        for (proto, port) in [("tcp", ports.tcp), ("udp", ports.udp)] {
            checked(
                program,
                &[
                    "-A",
                    &chain,
                    "-p",
                    proto,
                    "--dport",
                    &port.to_string(),
                    "-j",
                    "ACCEPT",
                ],
            )
            .await?;
        }
        info!("Updated {program} peer ports on {interface}");
    }
    Ok(())
}

pub async fn manage_firewall(
    interface: &str,
    ipv6: bool,
    ports: Option<&Ports>,
) -> eyre::Result<()> {
    update("iptables", interface, ports).await?;
    if ipv6 {
        update("ip6tables", interface, ports).await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        fs,
        os::unix::fs::PermissionsExt,
        sync::atomic::{AtomicU64, Ordering},
    };

    static NEXT_ID: AtomicU64 = AtomicU64::new(0);

    #[test]
    fn chain_is_interface_specific_and_within_iptables_limit() {
        assert_eq!(chain_name("trvpn0"), "OURO-trvpn0");
        assert!(chain_name("123456789012345").len() <= 28);
        assert_ne!(chain_name("trvpn0"), chain_name("other0"));
    }

    #[tokio::test]
    async fn prepares_and_updates_only_the_owned_chain() {
        let dir = std::env::temp_dir().join(format!(
            "ouro-firewall-test-{}-{}",
            std::process::id(),
            NEXT_ID.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&dir).unwrap();
        let program = dir.join("iptables");
        let log = dir.join("calls");
        fs::write(
            &program,
            format!(
                "#!/bin/sh\nprintf '%s\\n' \"$*\" >> '{}'\ncase \"$1\" in -S|-C) exit 1;; esac\n",
                log.display()
            ),
        )
        .unwrap();
        fs::set_permissions(&program, fs::Permissions::from_mode(0o700)).unwrap();
        let program = program.to_str().unwrap();
        prepare(program, "trvpn0").await.unwrap();
        update(
            program,
            "trvpn0",
            Some(&Ports {
                tcp: 1234,
                udp: 5678,
            }),
        )
        .await
        .unwrap();
        update(
            program,
            "trvpn0",
            Some(&Ports {
                tcp: 2222,
                udp: 2222,
            }),
        )
        .await
        .unwrap();
        update(program, "trvpn0", None).await.unwrap();
        let calls = fs::read_to_string(&log).unwrap();
        assert_eq!(
            calls,
            "-S OURO-trvpn0\n-N OURO-trvpn0\n-F OURO-trvpn0\n-C INPUT -i trvpn0 -j OURO-trvpn0\n-A INPUT -i trvpn0 -j OURO-trvpn0\n-F OURO-trvpn0\n-A OURO-trvpn0 -p tcp --dport 1234 -j ACCEPT\n-A OURO-trvpn0 -p udp --dport 5678 -j ACCEPT\n-F OURO-trvpn0\n-A OURO-trvpn0 -p tcp --dport 2222 -j ACCEPT\n-A OURO-trvpn0 -p udp --dport 2222 -j ACCEPT\n-F OURO-trvpn0\n"
        );
        fs::remove_file(&log).unwrap();
        fs::remove_file(&program).unwrap();
        fs::remove_dir(&dir).unwrap();
    }

    #[tokio::test]
    async fn failed_command_is_reported() {
        assert!(checked("false", &["test"]).await.is_err());
    }
}
