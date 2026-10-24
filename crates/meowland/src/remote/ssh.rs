//! Native SSH connection setup, pinned host keys, and public-key
//! authentication.

use std::{
    io,
    path::PathBuf,
    sync::{Arc, Mutex},
};

use russh::{
    client,
    keys::{
        self, PrivateKeyWithHashAlg,
        agent::{AgentIdentity, client::AgentClient},
    },
};

use super::wire;

type SshError = Box<dyn std::error::Error + Send + Sync>;

pub(super) struct Client {
    host: String,
    port: u16,
    known_hosts: PathBuf,
    stderr: Arc<Mutex<String>>,
}

impl Client {
    fn verify_host(&self, key: &keys::PublicKeyOrCertificate) -> Result<bool, SshError> {
        let key = key.public_key();
        if !keys::check_known_hosts_path(&self.host, self.port, &key, &self.known_hosts)? {
            return Err(io::Error::new(io::ErrorKind::PermissionDenied, format!(
                "unknown SSH host key for {}:{} ({}); verify it and add it to {} before connecting",
                self.host, self.port, key.fingerprint(keys::HashAlg::Sha256), self.known_hosts.display()
            )).into());
        }
        Ok(true)
    }

    fn record_diagnostic(&self, data: &[u8]) -> Result<(), SshError> {
        let mut stderr = self
            .stderr
            .lock()
            .map_err(|_| io::Error::other("SSH diagnostic lock poisoned"))?;
        let remaining = 512usize.saturating_sub(stderr.chars().count());
        stderr.extend(
            String::from_utf8_lossy(data)
                .chars()
                .filter(|ch| !ch.is_control())
                .take(remaining),
        );
        drop(stderr);
        Ok(())
    }
}

impl client::Handler for Client {
    type Error = SshError;

    fn check_server_key(
        &mut self,
        key: &keys::PublicKeyOrCertificate,
    ) -> impl Future<Output = Result<bool, Self::Error>> + Send {
        std::future::ready(self.verify_host(key))
    }

    fn extended_data(
        &mut self,
        _channel: russh::ChannelId,
        _ext: u32,
        data: &[u8],
        _session: &mut client::Session,
    ) -> impl Future<Output = Result<(), Self::Error>> + Send {
        std::future::ready(self.record_diagnostic(data))
    }
}

pub(super) async fn connect(
    host: &str,
) -> io::Result<(client::Handle<Client>, russh::ChannelStream<client::Msg>)> {
    let config = host_config(host)?;
    let home =
        std::env::home_dir().ok_or_else(|| io::Error::other("cannot locate home directory"))?;
    let stderr = Arc::new(Mutex::new(String::new()));
    let handler = Client {
        host: config.host().to_owned(),
        port: config.port(),
        known_hosts: config
            .host_config
            .user_known_hosts_file
            .clone()
            .unwrap_or_else(|| home.join(".ssh/known_hosts")),
        stderr: Arc::clone(&stderr),
    };
    let options = client::Config {
        keepalive_interval: Some(std::time::Duration::from_secs(15)),
        keepalive_max: 3,
        ..Default::default()
    };
    let mut session = client::connect(Arc::new(options), (config.host(), config.port()), handler)
        .await
        .map_err(io::Error::other)?;
    authenticate(&mut session, &config, home).await?;
    let channel = session
        .channel_open_session()
        .await
        .map_err(io::Error::other)?;
    channel
        .exec(true, "meowland relay")
        .await
        .map_err(io::Error::other)?;
    let mut stream = channel.into_stream();
    let (mut reader, mut writer) = tokio::io::split(&mut stream);
    wire::greet(&mut reader, &mut writer)
        .await
        .map_err(|error| {
            let diagnostic = stderr.lock().map_or_default(|text| text.clone());
            io::Error::new(
                error.kind(),
                format!("remote relay handshake failed: {error}; {diagnostic}"),
            )
        })?;
    Ok((session, stream))
}

async fn authenticate(
    session: &mut client::Handle<Client>,
    config: &russh_config::Config,
    home: PathBuf,
) -> io::Result<()> {
    let user = config.user();
    let hash = session
        .best_supported_rsa_hash()
        .await
        .map_err(io::Error::other)?
        .flatten();
    if let Ok(mut agent) = AgentClient::connect_env().await {
        for identity in agent.request_identities().await.map_err(io::Error::other)? {
            if let AgentIdentity::PublicKey { key, .. } = identity {
                let result = session
                    .authenticate_publickey_with(&user, key, hash, &mut agent)
                    .await
                    .map_err(io::Error::other)?;
                if result.success() {
                    return Ok(());
                }
            }
        }
    }
    let identities = config.host_config.identity_file.clone().unwrap_or_else(|| {
        ["id_ed25519", "id_ecdsa", "id_rsa"]
            .map(|name| home.join(".ssh").join(name))
            .into()
    });
    for path in identities {
        if let Ok(key) = keys::load_secret_key(&path, None) {
            let result = session
                .authenticate_publickey(&user, PrivateKeyWithHashAlg::new(Arc::new(key), hash))
                .await
                .map_err(io::Error::other)?;
            if result.success() {
                return Ok(());
            }
        }
    }
    Err(io::Error::new(
        io::ErrorKind::PermissionDenied,
        "SSH public-key authentication failed; load your key into SSH_AUTH_SOCK or configure an unencrypted IdentityFile; password and certificate authentication are not supported",
    ))
}

fn host_config(destination: &str) -> io::Result<russh_config::Config> {
    let (user, destination) = destination
        .rsplit_once('@')
        .map_or((None, destination), |(user, host)| (Some(user), host));
    let (host, port) = if let Some(host) = destination.strip_prefix('[') {
        let (host, suffix) = host
            .split_once(']')
            .ok_or_else(|| invalid("invalid bracketed SSH host"))?;
        (
            host,
            if suffix.is_empty() {
                None
            } else {
                Some(parse_port(
                    suffix
                        .strip_prefix(':')
                        .ok_or_else(|| invalid("invalid SSH port"))?,
                )?)
            },
        )
    } else if destination.matches(':').count() == 1 {
        let (host, port) = destination
            .split_once(':')
            .ok_or_else(|| invalid("invalid SSH destination"))?;
        (host, Some(parse_port(port)?))
    } else {
        (destination, None)
    };
    if host.is_empty()
        || user.is_some_and(str::is_empty)
        || destination.chars().any(char::is_whitespace)
    {
        return Err(invalid(
            "expected [user@]host[:port] or an SSH config alias",
        ));
    }
    let mut config = match russh_config::parse_home(host) {
        Ok(config) => config,
        Err(russh_config::Error::Io(error)) if error.kind() == io::ErrorKind::NotFound => {
            russh_config::Config::default(host)
        }
        Err(error) => return Err(io::Error::other(error)),
    };
    if user.is_some() {
        config.user = user.map(str::to_owned);
    }
    if port.is_some() {
        config.host_config.port = port;
    }
    if config.host_config.proxy_jump.is_some() || config.host_config.proxy_command.is_some() {
        return Err(invalid(
            "ProxyJump and ProxyCommand are not supported by the native SSH transport",
        ));
    }
    Ok(config)
}

fn parse_port(port: &str) -> io::Result<u16> {
    port.parse()
        .ok()
        .filter(|port| *port != 0)
        .ok_or_else(|| invalid("invalid SSH port"))
}

fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message)
}
